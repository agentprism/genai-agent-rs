//! Google API-family handlers, transports, and provider registrations.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{
    GoogleDecodeContext, GoogleSseDecoder, google_auth_resolver, google_models,
    local_google_auth_resolver,
};
use agentprism_ai::{
    AiError, AiErrorKind, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, ApiRequestOptions,
    AssistantStream, AuthResolutionOverrides, CONTEXT_SAFETY_TOKENS, CancellationToken, ChatApi,
    Context, EncodeContext, ErasedApiFullOptions, ErasedApiHandler, ErasedApiOptionsPatch,
    GoogleCompat, GoogleGenerativeAi, GoogleHandoff, GoogleSimplePatch, GoogleVertex,
    HeaderMapSpec, HttpBody, HttpChatApi, HttpRequest, HttpResponse, HttpTransport,
    LocalApiExecutionContext, LocalAssistantStream, LocalBoxFuture, LocalChatApi,
    LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalProviderRegistration, LocalProviderResponseStream, MessageId, MiddlewareError,
    ModelDescriptor, OrderedJsonObject, OrderedJsonWriter, ProviderPayload, ProviderRegistration,
    ProviderRegistrationError, ProviderResponseStream, SecretString, SendBoxFuture,
    SimpleGenerationOptions, SimpleLoweringContext, Timestamp, TypedModelDescriptor,
    estimate_context_tokens, transform_context_for_model,
};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, Method, header};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoogleApiKind {
    Generative,
    Vertex,
}

/// Erased handler for one of the two Google API families.
#[derive(Clone, Debug)]
pub struct GoogleHandler {
    api: ApiId,
    kind: GoogleApiKind,
}

impl GoogleHandler {
    /// Creates a Gemini Developer API handler.
    pub fn generative() -> Self {
        Self {
            api: ApiId::new(GoogleGenerativeAi::API_ID),
            kind: GoogleApiKind::Generative,
        }
    }

    /// Creates a Vertex Gemini handler.
    pub fn vertex() -> Self {
        Self {
            api: ApiId::new(GoogleVertex::API_ID),
            kind: GoogleApiKind::Vertex,
        }
    }
}

impl ErasedApiHandler for GoogleHandler {
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
        let execution = EncodingExecution::from_send(execution);
        match self.kind {
            GoogleApiKind::Generative => {
                lower_and_encode::<GoogleGenerativeAi>(model, context, simple, patch, &execution)
            }
            GoogleApiKind::Vertex => {
                lower_and_encode::<GoogleVertex>(model, context, simple, patch, &execution)
            }
        }
    }

    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        let execution = EncodingExecution::from_send(execution);
        match self.kind {
            GoogleApiKind::Generative => {
                encode_full::<GoogleGenerativeAi>(model, context, options, &execution)
            }
            GoogleApiKind::Vertex => {
                encode_full::<GoogleVertex>(model, context, options, &execution)
            }
        }
    }

    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        apply_full_options_auth_overrides(self.kind, model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        decode_send_body(
            response.body,
            decode_context(execution.model, self.api.clone()),
            execution.cancellation.clone(),
        )
    }
}

impl LocalErasedApiHandler for GoogleHandler {
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
        let execution = EncodingExecution::from_local(execution);
        match self.kind {
            GoogleApiKind::Generative => {
                lower_and_encode::<GoogleGenerativeAi>(model, context, simple, patch, &execution)
            }
            GoogleApiKind::Vertex => {
                lower_and_encode::<GoogleVertex>(model, context, simple, patch, &execution)
            }
        }
    }

    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        let execution = EncodingExecution::from_local(execution);
        match self.kind {
            GoogleApiKind::Generative => {
                encode_full::<GoogleGenerativeAi>(model, context, options, &execution)
            }
            GoogleApiKind::Vertex => {
                encode_full::<GoogleVertex>(model, context, options, &execution)
            }
        }
    }

    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        apply_full_options_auth_overrides(self.kind, model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    fn decode_stream(
        &self,
        response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        decode_local_body(
            response.body,
            decode_context(execution.model, self.api.clone()),
            execution.cancellation.clone(),
        )
    }
}

fn apply_full_options_auth_overrides(
    kind: GoogleApiKind,
    model: &ModelDescriptor,
    options: &ErasedApiFullOptions,
    overrides: &mut AuthResolutionOverrides,
) -> Result<(), AiError> {
    if kind == GoogleApiKind::Generative {
        return Ok(());
    }
    let options = options.downcast_ref::<GoogleVertex>().ok_or_else(|| {
        invalid_request(
            model,
            format!("invalid {} full options type", GoogleVertex::API_ID),
        )
    })?;
    if let Some(project) = options.project.as_ref().filter(|value| !value.is_empty()) {
        overrides
            .environment
            .insert("GOOGLE_CLOUD_PROJECT".to_owned(), project.clone());
    }
    if let Some(location) = options.location.as_ref().filter(|value| !value.is_empty()) {
        overrides
            .environment
            .insert("GOOGLE_CLOUD_LOCATION".to_owned(), location.clone());
    }
    Ok(())
}

fn decode_send_body(
    body: HttpBody,
    context: GoogleDecodeContext,
    cancellation: CancellationToken,
) -> AssistantStream {
    let mut decoder = GoogleSseDecoder::new(context);
    let pending = decoder.take_events().into();
    AssistantStream::new(stream::unfold(
        SendDecodeState {
            body,
            decoder,
            cancellation,
            pending,
            done: false,
        },
        next_send_event,
    ))
}

fn decode_local_body(
    body: LocalHttpBody,
    context: GoogleDecodeContext,
    cancellation: CancellationToken,
) -> LocalAssistantStream {
    let mut decoder = GoogleSseDecoder::new(context);
    let pending = decoder.take_events().into();
    LocalAssistantStream::new(stream::unfold(
        LocalDecodeState {
            body,
            decoder,
            cancellation,
            pending,
            done: false,
        },
        next_local_event,
    ))
}

struct EncodingExecution<'a> {
    endpoint: &'a Url,
    headers: &'a HeaderMap,
    api_key: Option<&'a SecretString>,
}

impl<'a> EncodingExecution<'a> {
    fn from_send(execution: &'a ApiExecutionContext<'a>) -> Self {
        Self {
            endpoint: execution.endpoint,
            headers: execution.headers,
            api_key: execution.api_key,
        }
    }

    fn from_local(execution: &'a LocalApiExecutionContext<'a>) -> Self {
        Self {
            endpoint: execution.endpoint,
            headers: execution.headers,
            api_key: execution.api_key,
        }
    }
}

trait GoogleFamily:
    ApiFamily<
        Compat = GoogleCompat,
        ModelConfig = agentprism_ai::GoogleModelConfig,
        OptionsPatch = GoogleSimplePatch,
        WireRequest = OrderedJsonObject,
    >
{
    fn config(model: &ModelDescriptor) -> Option<&agentprism_ai::GoogleModelConfig>;

    fn transport_route(
        model: &ModelDescriptor,
        effective_endpoint: &Url,
        options: &Self::FullOptions,
    ) -> GoogleTransportRoute;
}

impl GoogleFamily for GoogleGenerativeAi {
    fn config(model: &ModelDescriptor) -> Option<&agentprism_ai::GoogleModelConfig> {
        match &model.api {
            ApiModelConfig::GoogleGenerativeAi(config) => Some(config),
            _ => None,
        }
    }

    fn transport_route(
        model: &ModelDescriptor,
        _effective_endpoint: &Url,
        _options: &Self::FullOptions,
    ) -> GoogleTransportRoute {
        GoogleTransportRoute {
            model: model.common.model_ref.model.as_str().to_owned(),
            project: None,
            location: None,
            collection_scoped_base_url: false,
        }
    }
}

impl GoogleFamily for GoogleVertex {
    fn config(model: &ModelDescriptor) -> Option<&agentprism_ai::GoogleModelConfig> {
        match &model.api {
            ApiModelConfig::GoogleVertex(config) => Some(config),
            _ => None,
        }
    }

    fn transport_route(
        model: &ModelDescriptor,
        effective_endpoint: &Url,
        options: &Self::FullOptions,
    ) -> GoogleTransportRoute {
        GoogleTransportRoute {
            model: model.common.model_ref.model.as_str().to_owned(),
            project: options.project.clone(),
            location: options.location.clone(),
            collection_scoped_base_url: is_custom_vertex_base_url(model, effective_endpoint),
        }
    }
}

fn is_custom_vertex_base_url(model: &ModelDescriptor, effective_endpoint: &Url) -> bool {
    let catalog_default = Url::parse("https://us-central1-aiplatform.googleapis.com")
        .expect("static Vertex catalog endpoint URL");
    model.common.base_url != catalog_default && model.common.base_url == *effective_endpoint
}

fn lower_and_encode<A: GoogleFamily>(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    execution: &EncodingExecution<'_>,
) -> Result<ProviderPayload, AiError> {
    assert_request_auth::<A>(model, execution.api_key, execution.headers)?;
    let typed = typed_model::<A>(model)?;
    let compatibility = A::resolve_compat(execution.endpoint, &GoogleCompat::default())
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let (estimated, available) = if model.common.limits.context_window == 0 {
        (0, 0)
    } else {
        let estimated = estimate_context_tokens(context)
            .map_err(|error| invalid_request(model, error.to_string()))?
            .tokens;
        (
            estimated,
            model
                .common
                .limits
                .context_window
                .saturating_sub(estimated)
                .saturating_sub(CONTEXT_SAFETY_TOKENS),
        )
    };
    let patch = parse_patch::<A>(model, patch)?;
    let options = A::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compatibility,
            effective_base_url: execution.endpoint,
            estimated_input_tokens: estimated,
            available_context_tokens: available,
        },
        simple,
        &patch,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    encode_options::<A>(
        model,
        context,
        execution.endpoint,
        typed,
        compatibility,
        &options,
    )
}

fn encode_full<A: GoogleFamily>(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    execution: &EncodingExecution<'_>,
) -> Result<ProviderPayload, AiError> {
    assert_request_auth::<A>(model, execution.api_key, execution.headers)?;
    let options = options.downcast_ref::<A>().ok_or_else(|| {
        invalid_request(model, format!("invalid {} full options type", A::API_ID))
    })?;
    let typed = typed_model::<A>(model)?;
    let compatibility = A::resolve_compat(execution.endpoint, &GoogleCompat::default())
        .map_err(|error| invalid_request(model, error.to_string()))?;
    encode_options::<A>(
        model,
        context,
        execution.endpoint,
        typed,
        compatibility,
        options,
    )
}

fn typed_model<A: GoogleFamily>(
    model: &ModelDescriptor,
) -> Result<TypedModelDescriptor<A>, AiError> {
    let config = A::config(model).ok_or_else(|| {
        invalid_request(
            model,
            format!("model uses API {}, not {}", model.api.api_id(), A::API_ID),
        )
    })?;
    Ok(TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    })
}

fn encode_options<A: GoogleFamily>(
    model: &ModelDescriptor,
    context: &Context,
    endpoint: &Url,
    typed: TypedModelDescriptor<A>,
    compatibility: GoogleCompat,
    options: &A::FullOptions,
) -> Result<ProviderPayload, AiError> {
    let projected =
        transform_context_for_model(context, model, &Default::default(), &GoogleHandoff)
            .map_err(|error| invalid_request(model, error.to_string()))?
            .context;
    let wire = A::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compatibility,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let route =
        serde_json::to_string(&A::transport_route(model, endpoint, options)).map_err(|error| {
            invalid_request(model, format!("failed to encode Google route: {error}"))
        })?;
    Ok(
        ProviderPayload::typed::<A, _>(Method::POST, typed, wire, |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode Google payload: {error}"),
                )
            })
        })
        .with_transport_session_id(Some(route)),
    )
}

fn parse_patch<A: GoogleFamily>(
    model: &ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<GoogleSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(GoogleSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported {} options schema version {}",
                A::API_ID,
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn assert_request_auth<A: GoogleFamily>(
    model: &ModelDescriptor,
    api_key: Option<&SecretString>,
    headers: &HeaderMap,
) -> Result<(), AiError> {
    let has_api_key = api_key.is_some_and(|key| !key.expose_secret().trim().is_empty());
    let has_vertex_bearer = A::API_ID == GoogleVertex::API_ID
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.trim().is_empty());
    if has_api_key || has_vertex_bearer {
        return Ok(());
    }
    Err(AiError::new(
        AiErrorKind::Authentication,
        format!(
            "No API key for provider: {}",
            model.common.model_ref.provider
        ),
    )
    .with_model(model.common.model_ref.clone()))
}

fn invalid_request(model: &ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

fn decode_context(model: &ModelDescriptor, api: ApiId) -> GoogleDecodeContext {
    GoogleDecodeContext {
        message_id: MessageId::new(format!(
            "google-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        api,
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: now_timestamp(),
    }
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

struct SendDecodeState {
    body: HttpBody,
    decoder: GoogleSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeState {
    body: LocalHttpBody,
    decoder: GoogleSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, agentprism_ai::TransportError>>),
}

async fn next_send_event(
    mut state: SendDecodeState,
) -> Option<(agentprism_ai::AssistantEvent, SendDecodeState)> {
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
                    .extend(state.decoder.fail_transport(error.code, error.message));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_local_event(
    mut state: LocalDecodeState,
) -> Option<(agentprism_ai::AssistantEvent, LocalDecodeState)> {
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
                    .extend(state.decoder.fail_transport(error.code, error.message));
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

/// Creates a shared Gemini Developer API object.
pub fn google_generative_ai_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(GoogleHandler::generative()),
        Arc::new(GoogleTransport::new(transport, GoogleApiKind::Generative)),
    ))
}

/// Creates a shared Vertex Gemini API object.
pub fn google_vertex_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(GoogleHandler::vertex()),
        Arc::new(GoogleTransport::new(transport, GoogleApiKind::Vertex)),
    ))
}

/// Creates a local Gemini Developer API object.
pub fn local_google_generative_ai_api(
    transport: Rc<dyn LocalHttpTransport>,
) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(GoogleHandler::generative()),
        Rc::new(LocalGoogleTransport::new(
            transport,
            GoogleApiKind::Generative,
        )),
    ))
}

/// Creates a local Vertex Gemini API object.
pub fn local_google_vertex_api(transport: Rc<dyn LocalHttpTransport>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(GoogleHandler::vertex()),
        Rc::new(LocalGoogleTransport::new(transport, GoogleApiKind::Vertex)),
    ))
}

/// Transport decorator that inserts the model-specific GenerateContent path.
#[derive(Clone)]
pub struct GoogleTransport {
    inner: Arc<dyn HttpTransport>,
    kind: GoogleApiKind,
}

impl GoogleTransport {
    fn new(inner: Arc<dyn HttpTransport>, kind: GoogleApiKind) -> Self {
        Self { inner, kind }
    }
}

impl fmt::Debug for GoogleTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for GoogleTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, agentprism_ai::TransportError>> {
        if let Some(route) = request.session_id.take() {
            let route = match decode_transport_route(&route) {
                Ok(route) => route,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
            request.url =
                match generate_content_url(&request.url, &request.headers, &route, self.kind) {
                    Ok(url) => url,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
        }
        self.inner.execute(request, cancellation)
    }
}

/// Local counterpart to [`GoogleTransport`].
#[derive(Clone)]
pub struct LocalGoogleTransport {
    inner: Rc<dyn LocalHttpTransport>,
    kind: GoogleApiKind,
}

impl LocalGoogleTransport {
    fn new(inner: Rc<dyn LocalHttpTransport>, kind: GoogleApiKind) -> Self {
        Self { inner, kind }
    }
}

impl fmt::Debug for LocalGoogleTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalGoogleTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalGoogleTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, agentprism_ai::TransportError>> {
        if let Some(route) = request.session_id.take() {
            let route = match decode_transport_route(&route) {
                Ok(route) => route,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
            request.url =
                match generate_content_url(&request.url, &request.headers, &route, self.kind) {
                    Ok(url) => url,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
        }
        self.inner.execute(request, cancellation)
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct GoogleTransportRoute {
    model: String,
    project: Option<String>,
    location: Option<String>,
    collection_scoped_base_url: bool,
}

fn decode_transport_route(
    route: &str,
) -> Result<GoogleTransportRoute, agentprism_ai::TransportError> {
    serde_json::from_str(route).map_err(|_| {
        agentprism_ai::TransportError::new(
            "invalid_google_transport_route",
            "Google transport routing metadata was invalid",
        )
    })
}

fn generate_content_url(
    base: &Url,
    headers: &HeaderMap,
    route: &GoogleTransportRoute,
    kind: GoogleApiKind,
) -> Result<Url, agentprism_ai::TransportError> {
    validate_google_model(&route.model)?;
    let mut url = base.clone();
    let mut path = base.path().trim_end_matches('/').to_owned();
    match kind {
        GoogleApiKind::Generative => {
            path.push('/');
            if route.model.starts_with("models/") || route.model.starts_with("tunedModels/") {
                path.push_str(&route.model);
            } else {
                path.push_str("models/");
                path.push_str(&route.model);
            }
            path.push_str(":streamGenerateContent");
        }
        GoogleApiKind::Vertex => {
            if !has_google_api_key(headers)
                && !route.collection_scoped_base_url
                && is_standard_vertex_endpoint(&url)
            {
                let (endpoint_project, endpoint_location) = vertex_resource_scope(&path);
                let project = route.project.as_deref().or(endpoint_project.as_deref());
                let location = route.location.as_deref().or(endpoint_location.as_deref());
                let (Some(project), Some(location)) = (project, location) else {
                    return Err(agentprism_ai::TransportError::new(
                        "invalid_vertex_scope",
                        "Vertex AI requires both project and location",
                    ));
                };
                url.set_host(Some(&vertex_host(location))).map_err(|_| {
                    agentprism_ai::TransportError::new(
                        "invalid_vertex_endpoint",
                        "Vertex location could not be encoded in the endpoint",
                    )
                })?;
                path = vertex_scope_prefix(&path);
                if !path_contains_api_version(&path) {
                    path.push_str("/v1");
                }
                path.push_str("/projects/");
                path.push_str(project);
                path.push_str("/locations/");
                path.push_str(location);
            }
            if !path_contains_api_version(&path) {
                path.push_str("/v1");
            }
            path.push('/');
            path.push_str(&vertex_model_path(&route.model));
            path.push_str(":streamGenerateContent");
        }
    }
    url.set_path(&path);
    url.set_query(Some("alt=sse"));
    Ok(url)
}

fn validate_google_model(model: &str) -> Result<(), agentprism_ai::TransportError> {
    if model.is_empty() {
        return Err(agentprism_ai::TransportError::new(
            "invalid_google_model",
            "model is required and must be a string",
        ));
    }
    if ["..", "?", "&"]
        .into_iter()
        .any(|token| model.contains(token))
    {
        return Err(agentprism_ai::TransportError::new(
            "invalid_google_model",
            "invalid model parameter",
        ));
    }
    Ok(())
}

fn has_google_api_key(headers: &HeaderMap) -> bool {
    headers
        .get("x-goog-api-key")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

fn is_standard_vertex_endpoint(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "aiplatform.googleapis.com"
            || host.ends_with("-aiplatform.googleapis.com")
            || (host.starts_with("aiplatform.") && host.ends_with(".rep.googleapis.com"))
    })
}

fn vertex_resource_scope(path: &str) -> (Option<String>, Option<String>) {
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let project = components
        .windows(2)
        .find_map(|pair| (pair[0] == "projects").then(|| pair[1].to_owned()));
    let location = components
        .windows(2)
        .find_map(|pair| (pair[0] == "locations").then(|| pair[1].to_owned()));
    (project, location)
}

fn vertex_scope_prefix(path: &str) -> String {
    path.find("/projects/").map_or_else(
        || path.trim_end_matches('/').to_owned(),
        |index| path[..index].to_owned(),
    )
}

fn vertex_host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    }
}

fn vertex_model_path(model: &str) -> String {
    if ["publishers/", "projects/", "models/"]
        .into_iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return model.to_owned();
    }
    if let Some((publisher, remainder)) = model.split_once('/') {
        let name = remainder.split('/').next().unwrap_or_default();
        return format!("publishers/{publisher}/models/{name}");
    }
    format!("publishers/google/models/{model}")
}

fn path_contains_api_version(path: &str) -> bool {
    path.split('/').any(|component| {
        let Some(version) = component.strip_prefix('v') else {
            return false;
        };
        let digits = version.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        let suffix = &version[digits..];
        suffix.is_empty()
            || suffix
                .strip_prefix("beta")
                .is_some_and(|beta| beta.chars().all(|character| character.is_ascii_digit()))
    })
}

/// Pi-compatible logical request defaults for both Google APIs.
pub fn google_default_headers() -> HeaderMapSpec {
    HeaderMapSpec::from([
        ("accept".to_owned(), Some("*/*".to_owned())),
        (
            "content-type".to_owned(),
            Some("application/json".to_owned()),
        ),
        ("user-agent".to_owned(), Some(google_user_agent())),
    ])
}

/// Returns pinned Pi's platform-shaped user-agent string.
pub fn google_user_agent() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        "pi (browser)".to_owned()
    }
    #[cfg(all(not(target_arch = "wasm32"), unix))]
    {
        let system = rustix::system::uname();
        let platform = match system.sysname().to_string_lossy().as_ref() {
            "Darwin" => "darwin".to_owned(),
            value => value.to_ascii_lowercase(),
        };
        let release = system.release().to_string_lossy();
        let architecture = match system.machine().to_string_lossy().as_ref() {
            "x86_64" => "x64".to_owned(),
            "aarch64" | "arm64" => "arm64".to_owned(),
            value => value.to_owned(),
        };
        format!("pi ({platform} {release}; {architecture})")
    }
    #[cfg(all(not(target_arch = "wasm32"), windows))]
    {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>())
                .expect("OSVERSIONINFOW size fits u32"),
            ..OSVERSIONINFOW::default()
        };
        // SAFETY: the initialized structure has the size required by Windows.
        let status = unsafe { RtlGetVersion(&mut version) };
        let release = if status >= 0 {
            format!(
                "{}.{}.{}",
                version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
            )
        } else {
            "unknown".to_owned()
        };
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            value => value,
        };
        format!("pi (win32 {release}; {architecture})")
    }
    #[cfg(all(not(target_arch = "wasm32"), not(unix), not(windows)))]
    {
        format!(
            "pi ({} unknown; {})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }
}

/// Builds the built-in Google provider registration.
pub fn google_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, GoogleProviderError> {
    ProviderRegistration::builder("google")
        .display_name("Google")
        .base_url(
            Url::parse("https://generativelanguage.googleapis.com/v1beta")
                .map_err(GoogleProviderError::Url)?,
        )
        .headers(google_default_headers())
        .auth(google_auth_resolver())
        .models(google_models().map_err(GoogleProviderError::Catalog)?)
        .api(
            GoogleGenerativeAi::API_ID,
            google_generative_ai_api(transport),
        )
        .build()
        .map_err(GoogleProviderError::Registration)
}

/// Builds the local Google provider registration.
pub fn local_google_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, GoogleProviderError> {
    LocalProviderRegistration::builder("google")
        .display_name("Google")
        .base_url(
            Url::parse("https://generativelanguage.googleapis.com/v1beta")
                .map_err(GoogleProviderError::Url)?,
        )
        .headers(google_default_headers())
        .auth(local_google_auth_resolver())
        .models(google_models().map_err(GoogleProviderError::Catalog)?)
        .api(
            GoogleGenerativeAi::API_ID,
            local_google_generative_ai_api(transport),
        )
        .build()
        .map_err(GoogleProviderError::Registration)
}

/// Error while building a built-in Google registration.
#[derive(Debug)]
pub enum GoogleProviderError {
    /// Pinned catalog data was invalid.
    Catalog(crate::GoogleCatalogError),
    /// A built-in URL was invalid.
    Url(url::ParseError),
    /// Provider registration invariants failed.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for GoogleProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
            Self::Url(error) => write!(formatter, "URL error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for GoogleProviderError {}

#[cfg(test)]
mod tests {
    use super::*;
    use agentprism_ai::{
        AssistantEvent, ContentBlock, ModelId, ModelPricing, ProviderId, TokenPriceRates,
        ToolCallId,
    };

    const FIXED_TIMESTAMP: i64 = 1_700_000_000_000;
    const TOOL_CALL_SSE: &[u8] = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"Cargo.toml\"}}}]},\"finishReason\":\"STOP\"}]}\n\n";

    fn fixed_decode_context(api: &str, provider: &str, message_id: &str) -> GoogleDecodeContext {
        GoogleDecodeContext {
            message_id: MessageId::new(message_id),
            provider: ProviderId::new(provider),
            api: ApiId::new(api),
            requested_model: ModelId::new("gemini-test"),
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: Default::default(),
            },
            timestamp: Timestamp::from_unix_millis(FIXED_TIMESTAMP),
        }
    }

    fn terminal_tool_call_id(events: &[AssistantEvent]) -> ToolCallId {
        let message = events
            .iter()
            .find_map(AssistantEvent::terminal_message)
            .expect("terminal Google assistant message");
        assert_eq!(message.timestamp.unix_millis(), FIXED_TIMESTAMP);
        message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall { call, .. } => Some(call.id.clone()),
                _ => None,
            })
            .expect("fallback Google tool-call ID")
    }

    fn collect_send(api: &str, provider: &str, message_id: &str) -> Vec<AssistantEvent> {
        let response = HttpResponse::from_bytes(200, HeaderMap::new(), TOOL_CALL_SSE.to_vec());
        futures_executor::block_on(
            decode_send_body(
                response.body,
                fixed_decode_context(api, provider, message_id),
                CancellationToken::new(),
            )
            .collect(),
        )
    }

    fn collect_local(api: &str, provider: &str, message_id: &str) -> Vec<AssistantEvent> {
        let response = LocalHttpResponse::from_bytes(200, HeaderMap::new(), TOOL_CALL_SSE.to_vec());
        futures_executor::block_on(
            decode_local_body(
                response.body,
                fixed_decode_context(api, provider, message_id),
                CancellationToken::new(),
            )
            .collect(),
        )
    }

    fn assert_separate_decoder_ids(decode: impl Fn(&str, &str, &str) -> Vec<AssistantEvent>) {
        for (api, provider) in [
            (GoogleGenerativeAi::API_ID, "google"),
            (GoogleVertex::API_ID, "google-vertex"),
        ] {
            let first = decode(api, provider, &format!("{api}-first"));
            let second = decode(api, provider, &format!("{api}-second"));
            let first_id = terminal_tool_call_id(&first);
            let second_id = terminal_tool_call_id(&second);
            assert_ne!(
                first_id, second_id,
                "separate {api} decoders at one timestamp must not reuse a fallback ID"
            );
            assert!(first_id.as_str().starts_with("read_file_1700000000000_"));
            assert!(second_id.as_str().starts_with("read_file_1700000000000_"));
        }
    }

    /// Architecture v2 part 2 §1.8 and §9.2; pinned Pi basis:
    /// `google-generative-ai.ts:50-51,187-193` and
    /// `google-vertex.ts:68-69,205-210`.
    #[test]
    fn google_send_tool_call_fallback_ids_are_unique_across_equal_timestamp_decoders() {
        assert_separate_decoder_ids(collect_send);
    }

    /// Architecture v2 part 2 §1.8 and §9.2; pinned Pi basis:
    /// `google-generative-ai.ts:50-51,187-193` and
    /// `google-vertex.ts:68-69,205-210`.
    #[test]
    fn google_local_tool_call_fallback_ids_are_unique_across_equal_timestamp_decoders() {
        assert_separate_decoder_ids(collect_local);
    }
}
