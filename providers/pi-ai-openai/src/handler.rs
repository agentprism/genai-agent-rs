//! Shared erased handler, HTTP adapters, authentication, and registrations.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{
    OpenAiCompletionsDecodeContext, OpenAiCompletionsSseDecoder, deepseek_models, openrouter_models,
};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    AiError, AiErrorKind, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, AssistantStream,
    AuthError, AuthInteraction, AuthResolver, CONTEXT_SAFETY_TOKENS, CancellationToken, ChatApi,
    Context, EncodeContext, EnvironmentApiKeyAuth, ErasedApiHandler, ErasedApiOptionsPatch,
    HttpBody, HttpChatApi, HttpRequest, HttpResponse, HttpTransport, LocalApiExecutionContext,
    LocalAssistantStream, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalChatApi,
    LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalOAuthAuth, LocalProviderAuthResolver, LocalProviderRegistration,
    LocalProviderResponseStream, LocalResolveAuthRequest, MessageId, MiddlewareError,
    ModelDescriptor, OAuthAuth, OpenAiCompletions, OpenAiCompletionsHandoff,
    OpenAiCompletionsSimplePatch, OrderedJsonWriter, ProviderAuthResolver, ProviderPayload,
    ProviderRegistration, ProviderRegistrationError, ProviderResponseStream, ResolveAuthRequest,
    ResolvedAuth, SendBoxFuture, SimpleGenerationOptions, SimpleLoweringContext, Timestamp,
    TypedModelDescriptor, estimate_context_tokens, openai_grammar_tool_input_properties,
    transform_context_for_model,
};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Shared OpenAI Chat Completions erased API-family implementation.
#[derive(Clone, Debug)]
pub struct OpenAiCompletionsHandler {
    api: ApiId,
}

impl Default for OpenAiCompletionsHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(OpenAiCompletions::API_ID),
        }
    }
}

impl ErasedApiHandler for OpenAiCompletionsHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(model, context, simple, patch, execution.endpoint)
    }

    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        let decode_context = decode_context(execution.model, execution.context, execution.endpoint);
        let mut decoder = OpenAiCompletionsSseDecoder::new(decode_context);
        let pending = decoder.take_events().into();
        AssistantStream::new(stream::unfold(
            SendDecodeStreamState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_send_decoded_event,
        ))
    }
}

impl LocalErasedApiHandler for OpenAiCompletionsHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(model, context, simple, patch, execution.endpoint)
    }

    fn decode_stream(
        &self,
        response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        let decode_context = decode_context(execution.model, execution.context, execution.endpoint);
        let mut decoder = OpenAiCompletionsSseDecoder::new(decode_context);
        let pending = decoder.take_events().into();
        LocalAssistantStream::new(stream::unfold(
            LocalDecodeStreamState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_local_decoded_event,
        ))
    }
}

struct SendDecodeStreamState {
    body: HttpBody,
    decoder: OpenAiCompletionsSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeStreamState {
    body: LocalHttpBody,
    decoder: OpenAiCompletionsSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, pi_ai::TransportError>>),
}

async fn next_send_decoded_event(
    mut state: SendDecodeStreamState,
) -> Option<(pi_ai::AssistantEvent, SendDecodeStreamState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_send_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(error))) => {
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_local_decoded_event(
    mut state: LocalDecodeStreamState,
) -> Option<(pi_ai::AssistantEvent, LocalDecodeStreamState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_local_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(error))) => {
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_send_body(body: &mut HttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

async fn next_local_body(body: &mut LocalHttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

fn lower_and_encode(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not openai-completions",
                model.api.api_id()
            ),
        ));
    };
    let typed = TypedModelDescriptor::<OpenAiCompletions> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compat = OpenAiCompletions::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &OpenAiCompletionsHandoff,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?
    .context;
    let estimate = estimate_context_tokens(&projected)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let available = model
        .common
        .limits
        .context_window
        .saturating_sub(estimate.tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    let patch = parse_patch(model, patch)?;
    let options = OpenAiCompletions::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compat,
            effective_base_url: endpoint,
            estimated_input_tokens: estimate.tokens,
            available_context_tokens: available,
        },
        simple,
        &patch,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let wire = OpenAiCompletions::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: endpoint,
        },
        &options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    Ok(ProviderPayload::typed::<OpenAiCompletions, _>(
        Method::POST,
        typed,
        wire,
        |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode OpenAI payload: {error}"),
                )
            })
        },
    ))
}

fn parse_patch(
    model: &ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<OpenAiCompletionsSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(OpenAiCompletionsSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported openai-completions options schema version {}",
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn invalid_request(model: &ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

fn decode_context(
    model: &ModelDescriptor,
    context: &Context,
    endpoint: &Url,
) -> OpenAiCompletionsDecodeContext {
    let (supports_finish_reason, grammar_tool_input_properties) = match &model.api {
        ApiModelConfig::OpenAiCompletions(config) => {
            let compat = OpenAiCompletions::resolve_compat(endpoint, &config.compat)
                .expect("compatibility was resolved during request lowering");
            let grammar_tool_input_properties =
                openai_grammar_tool_input_properties(context, &compat)
                    .expect("grammar tools were validated during request encoding");
            (
                compat.supports_finish_reason.unwrap_or(true),
                grammar_tool_input_properties,
            )
        }
        _ => (true, Default::default()),
    };
    OpenAiCompletionsDecodeContext {
        message_id: MessageId::new(format!(
            "openai-chat-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        timestamp: now_timestamp(),
        supports_finish_reason,
        grammar_tool_input_properties,
    }
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

/// Creates one shared Send API object for every OpenAI-compatible provider.
pub fn openai_completions_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(OpenAiCompletionsHandler::default()),
        Arc::new(OpenAiCompletionsTransport::new(transport)),
    ))
}

/// Creates one shared local-executor API object.
pub fn local_openai_completions_api(transport: Rc<dyn LocalHttpTransport>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(OpenAiCompletionsHandler::default()),
        Rc::new(LocalOpenAiCompletionsTransport::new(transport)),
    ))
}

/// Transport decorator resolving a provider base URL to `/chat/completions`.
#[derive(Clone)]
pub struct OpenAiCompletionsTransport {
    inner: Arc<dyn HttpTransport>,
}

impl OpenAiCompletionsTransport {
    /// Wraps one injected provider transport.
    pub fn new(inner: Arc<dyn HttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for OpenAiCompletionsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompletionsTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for OpenAiCompletionsTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, pi_ai::TransportError>> {
        request.url = completions_url(&request.url);
        ensure_json_headers(&mut request.headers);
        self.inner.execute(request, cancellation)
    }
}

/// Local transport decorator resolving `/chat/completions`.
#[derive(Clone)]
pub struct LocalOpenAiCompletionsTransport {
    inner: Rc<dyn LocalHttpTransport>,
}

impl LocalOpenAiCompletionsTransport {
    /// Wraps one injected local provider transport.
    pub fn new(inner: Rc<dyn LocalHttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for LocalOpenAiCompletionsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalOpenAiCompletionsTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalOpenAiCompletionsTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, pi_ai::TransportError>> {
        request.url = completions_url(&request.url);
        ensure_json_headers(&mut request.headers);
        self.inner.execute(request, cancellation)
    }
}

fn completions_url(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/chat/completions"));
    url
}

fn ensure_json_headers(headers: &mut HeaderMap) {
    if !headers.contains_key(header::ACCEPT) {
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    }
    if !headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
}

/// Builds the DeepSeek registration around a caller-shared API object.
pub fn deepseek_provider_with_api(
    api: Arc<dyn ChatApi>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    provider_registration(
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com",
        "DEEPSEEK_API_KEY",
        deepseek_models()?,
        api,
        None,
    )
}

/// Builds the OpenRouter registration around the same API object.
pub fn openrouter_provider_with_api(
    api: Arc<dyn ChatApi>,
    oauth_transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    provider_registration(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
        openrouter_models()?,
        api,
        Some(Arc::new(crate::OpenRouterOAuth::new(oauth_transport))),
    )
}

/// Builds a complete DeepSeek registration from one raw transport.
pub fn deepseek_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    deepseek_provider_with_api(openai_completions_api(transport))
}

/// Builds a complete OpenRouter registration whose API and OAuth flow share
/// one injected raw transport.
pub fn openrouter_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    let api = openai_completions_api(Arc::clone(&transport));
    openrouter_provider_with_api(api, transport)
}

/// Builds a local DeepSeek registration around a caller-shared API object.
pub fn local_deepseek_provider_with_api(
    api: Rc<dyn LocalChatApi>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    local_provider_registration(
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com",
        "DEEPSEEK_API_KEY",
        deepseek_models()?,
        api,
        None,
    )
}

/// Builds a local OpenRouter registration around a caller-shared API object.
pub fn local_openrouter_provider_with_api(
    api: Rc<dyn LocalChatApi>,
    oauth_transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    local_provider_registration(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
        openrouter_models()?,
        api,
        Some(Rc::new(crate::LocalOpenRouterOAuth::new(oauth_transport))),
    )
}

/// Builds a complete local DeepSeek registration from one raw transport.
pub fn local_deepseek_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    local_deepseek_provider_with_api(local_openai_completions_api(transport))
}

/// Builds a complete local OpenRouter registration whose API and OAuth flow
/// share one injected local raw transport.
pub fn local_openrouter_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    let api = local_openai_completions_api(Rc::clone(&transport));
    local_openrouter_provider_with_api(api, transport)
}

fn provider_registration(
    id: &str,
    display_name: &str,
    base_url: &str,
    environment_variable: &str,
    models: Vec<ModelDescriptor>,
    api: Arc<dyn ChatApi>,
    oauth: Option<Arc<dyn OAuthAuth>>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    let auth = Arc::new(BearerAuthResolver::new(environment_variable, oauth));
    ProviderRegistration::builder(id)
        .display_name(display_name)
        .base_url(Url::parse(base_url).map_err(OpenAiProviderError::Url)?)
        .auth(auth)
        .models(models)
        .api(OpenAiCompletions::API_ID, api)
        .build()
        .map_err(OpenAiProviderError::Registration)
}

fn local_provider_registration(
    id: &str,
    display_name: &str,
    base_url: &str,
    environment_variable: &str,
    models: Vec<ModelDescriptor>,
    api: Rc<dyn LocalChatApi>,
    oauth: Option<Rc<dyn LocalOAuthAuth>>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    let auth = Rc::new(LocalBearerAuthResolver::new(environment_variable, oauth));
    LocalProviderRegistration::builder(id)
        .display_name(display_name)
        .base_url(Url::parse(base_url).map_err(OpenAiProviderError::Url)?)
        .auth(auth)
        .models(models)
        .api(OpenAiCompletions::API_ID, api)
        .build()
        .map_err(OpenAiProviderError::Registration)
}

/// Error while building a built-in OpenAI-compatible registration.
#[derive(Debug)]
pub enum OpenAiProviderError {
    /// Pinned catalog data was invalid.
    Catalog(crate::OpenAiCatalogError),
    /// A built-in endpoint URL was invalid.
    Url(url::ParseError),
    /// The assembled provider registration violated a core invariant.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for OpenAiProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
            Self::Url(error) => write!(formatter, "URL error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for OpenAiProviderError {}

impl From<crate::OpenAiCatalogError> for OpenAiProviderError {
    fn from(value: crate::OpenAiCatalogError) -> Self {
        Self::Catalog(value)
    }
}

struct BearerAuthResolver {
    inner: ProviderAuthResolver,
}

impl BearerAuthResolver {
    fn new(environment_variable: &str, oauth: Option<Arc<dyn OAuthAuth>>) -> Self {
        Self {
            inner: ProviderAuthResolver::new(
                Some(Arc::new(EnvironmentApiKeyAuth::new(
                    "API key",
                    [environment_variable],
                ))),
                oauth,
            ),
        }
    }
}

impl AuthResolver for BearerAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_bearer_header(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> SendBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

struct LocalBearerAuthResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalBearerAuthResolver {
    fn new(environment_variable: &str, oauth: Option<Rc<dyn LocalOAuthAuth>>) -> Self {
        Self {
            inner: LocalProviderAuthResolver::new(
                Some(Rc::new(EnvironmentApiKeyAuth::new(
                    "API key",
                    [environment_variable],
                ))),
                oauth,
            ),
        }
    }
}

impl LocalAuthResolver for LocalBearerAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_bearer_header(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

fn insert_bearer_header(resolved: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(api_key) = resolved.api_key.take() else {
        return Ok(());
    };
    let value = format!("Bearer {}", api_key.expose_secret());
    let value = HeaderValue::from_str(&value)
        .map_err(|_| AuthError::new("invalid_api_key", "API key cannot be encoded as a header"))?;
    resolved.headers.insert(header::AUTHORIZATION, value);
    Ok(())
}
