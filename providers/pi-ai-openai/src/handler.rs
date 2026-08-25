//! Shared erased handler, HTTP adapters, authentication, and registrations.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{OpenAiCompletionsDecodeContext, OpenAiCompletionsSseDecoder};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    AiError, AiErrorKind, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, AssistantStream,
    AttemptFailure, AuthError, AuthInteraction, AuthResolver, CONTEXT_SAFETY_TOKENS,
    CancellationToken, ChatApi, Context, DefaultRetryClassifier, EncodeContext,
    EnvironmentApiKeyAuth, ErasedApiHandler, ErasedApiOptionsPatch, HttpBody, HttpChatApi,
    HttpRequest, HttpResponse, HttpTransport, LocalApiExecutionContext, LocalAssistantStream,
    LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalChatApi,
    LocalDefaultRetryClassifier, LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi,
    LocalHttpResponse, LocalHttpTransport, LocalOAuthAuth, LocalProviderAuthResolver,
    LocalProviderRegistration, LocalProviderResponseStream, LocalResolveAuthRequest,
    LocalRetryClassifier, MessageId, MiddlewareError, ModelDescriptor, OAuthAuth,
    OpenAiCompletions, OpenAiCompletionsHandoff, OpenAiCompletionsSimplePatch, OrderedJsonWriter,
    ProviderAuthResolver, ProviderPayload, ProviderRegistration, ProviderRegistrationError,
    ProviderResponseStream, ResolveAuthRequest, ResolvedAuth, RetryClassifier, RetryDecision,
    RetryPolicy, SendBoxFuture, SimpleGenerationOptions, SimpleLoweringContext, Timestamp,
    TypedModelDescriptor, estimate_context_tokens, openai_grammar_tool_input_properties,
    transform_context_for_model,
};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

/// Builds the pinned OpenAI Responses provider.
pub fn openai_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    openai_provider_with_api(crate::openai_responses_api(transport))
}

/// Builds OpenAI around a caller-shared Responses API object.
pub fn openai_provider_with_api(
    responses_api: Arc<dyn ChatApi>,
) -> Result<ProviderRegistration, OpenAiProviderError> {
    ProviderRegistration::builder("openai")
        .display_name("OpenAI")
        .base_url(Url::parse("https://api.openai.com/v1").map_err(OpenAiProviderError::Url)?)
        .auth(Arc::new(BearerAuthResolver::new("OPENAI_API_KEY", None)))
        .models(crate::openai_models()?)
        .api(pi_ai::OpenAiResponses::API_ID, responses_api)
        .build()
        .map_err(OpenAiProviderError::Registration)
}

/// Codex's narrower retry classifier from pinned Pi's Responses transport.
#[derive(Debug, Default)]
pub struct OpenAiCodexRetryClassifier {
    default: DefaultRetryClassifier,
}

impl RetryClassifier for OpenAiCodexRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        codex_retryable_failure(failure).map_or(RetryDecision::DoNotRetry, |failure| {
            self.default.classify(&failure, policy)
        })
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        normalize_codex_terminal_failure(failure)
    }
}

/// Local-executor counterpart to [`OpenAiCodexRetryClassifier`].
#[derive(Debug, Default)]
pub struct LocalOpenAiCodexRetryClassifier {
    default: LocalDefaultRetryClassifier,
}

impl LocalRetryClassifier for LocalOpenAiCodexRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        codex_retryable_failure(failure).map_or(RetryDecision::DoNotRetry, |failure| {
            self.default.classify(&failure, policy)
        })
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        normalize_codex_terminal_failure(failure)
    }
}

/// Returns the Codex-specific default retry policy.
pub fn openai_codex_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 0,
        max_server_delay: Some(Duration::from_secs(60)),
        exponential_base: Duration::from_secs(1),
        exponential_cap: Duration::MAX,
        jitter_multiplier: 1.0..=1.0,
    }
}

fn codex_retryable_failure(failure: &AttemptFailure) -> Option<AttemptFailure> {
    match failure {
        AttemptFailure::Transport { source, .. } => {
            (!contains_usage_limit(&source.message)).then(|| failure.clone())
        }
        AttemptFailure::Timeout { .. } => Some(failure.clone()),
        AttemptFailure::Http {
            attempt,
            status,
            headers,
            observed_at,
            message,
        } => {
            if *status == 429 && is_terminal_codex_rate_limit(message) {
                return None;
            }
            let retryable_status = matches!(*status, 429 | 500 | 502 | 503 | 504);
            let retryable_response = retryable_status || is_transient_codex_error(message);
            let classification_message =
                codex_classification_error_message(*status, message, *observed_at);
            if !retryable_response && contains_usage_limit(&classification_message) {
                return None;
            }
            let mut headers = headers.clone();
            headers.remove("x-should-retry");
            // Pinned Codex first handles its response-status/text allowlist in
            // the non-success response branch. Every other parsed HTTP error
            // is then thrown into the surrounding catch, which retries it with
            // exponential backoff but does not consult response delay headers.
            if !retryable_response {
                headers.clear();
            }
            Some(AttemptFailure::http_at(
                *attempt,
                if retryable_status { *status } else { 429 },
                *headers,
                *observed_at,
                classification_message,
            ))
        }
        AttemptFailure::Middleware { .. }
        | AttemptFailure::Cancelled
        | AttemptFailure::RetryDelayTooLong { .. } => None,
        _ => None,
    }
}

fn normalize_codex_terminal_failure(failure: AttemptFailure) -> AttemptFailure {
    match failure {
        AttemptFailure::Http {
            attempt,
            status,
            headers,
            observed_at,
            message,
        } => AttemptFailure::http_at(
            attempt,
            status,
            *headers,
            observed_at,
            codex_public_error_message(status, &message, observed_at),
        ),
        other => other,
    }
}

fn codex_public_error_message(status: u16, raw: &str, observed_at: SystemTime) -> String {
    // The raw body is retry-classification input only. Terminal public errors
    // may retain a parsed provider message, but malformed/plain response bytes
    // never cross the public boundary.
    codex_error_message(status, raw, observed_at, "Request failed")
}

fn codex_classification_error_message(status: u16, raw: &str, observed_at: SystemTime) -> String {
    // Pinned Pi applies the catch-path `"usage limit"` exclusion to
    // `parseErrorResponse`'s result. For an unstructured body that result is
    // the raw text, but it must remain internal to retry classification.
    codex_error_message(status, raw, observed_at, raw)
}

fn codex_error_message(status: u16, raw: &str, observed_at: SystemTime, fallback: &str) -> String {
    let fallback = fallback.to_owned();
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        return fallback;
    };
    let Some(error) = parsed.get("error").and_then(serde_json::Value::as_object) else {
        return fallback;
    };
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let usage_code = [
        "usage_limit_reached",
        "usage_not_included",
        "rate_limit_exceeded",
    ]
    .into_iter()
    .any(|candidate| code.eq_ignore_ascii_case(candidate));
    if usage_code || status == 429 {
        let plan = error
            .get("plan_type")
            .and_then(serde_json::Value::as_str)
            .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
            .unwrap_or_default();
        let when = error
            .get("resets_at")
            .and_then(serde_json::Value::as_f64)
            .and_then(|reset_seconds| {
                let observed_millis =
                    observed_at.duration_since(UNIX_EPOCH).ok()?.as_secs_f64() * 1_000.0;
                let minutes = ((reset_seconds * 1_000.0 - observed_millis) / 60_000.0)
                    .round()
                    .max(0.0);
                Some(format!(" Try again in ~{minutes:.0} min."))
            })
            .unwrap_or_default();
        return format!("You have hit your ChatGPT usage limit{plan}.{when}")
            .trim()
            .to_owned();
    }
    error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.is_empty())
        .map_or(fallback, str::to_owned)
}

fn contains_usage_limit(message: &str) -> bool {
    message.contains("usage limit")
}

fn is_terminal_codex_rate_limit(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "you have hit your chatgpt usage limit",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ]
    .into_iter()
    .any(|pattern| message.contains(pattern))
}

fn is_transient_codex_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("overloaded")
        || contains_optional_separator(&message, "rate", "limit")
        || contains_optional_separator(&message, "service", "unavailable")
        || contains_optional_separator(&message, "upstream", "connect")
        || contains_optional_separator(&message, "connection", "refused")
}

fn contains_optional_separator(message: &str, prefix: &str, suffix: &str) -> bool {
    message.match_indices(prefix).any(|(index, _)| {
        let remainder = &message[index + prefix.len()..];
        remainder.starts_with(suffix)
            || remainder.chars().next().is_some_and(|separator| {
                !matches!(separator, '\n' | '\r' | '\u{2028}' | '\u{2029}')
                    && remainder[separator.len_utf8()..].starts_with(suffix)
            })
    })
}

/// Builds the local-executor OpenAI Responses provider.
pub fn local_openai_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    local_openai_provider_with_api(crate::local_openai_responses_api(transport))
}

/// Builds local OpenAI around a caller-shared Responses API object.
pub fn local_openai_provider_with_api(
    responses_api: Rc<dyn LocalChatApi>,
) -> Result<LocalProviderRegistration, OpenAiProviderError> {
    LocalProviderRegistration::builder("openai")
        .display_name("OpenAI")
        .base_url(Url::parse("https://api.openai.com/v1").map_err(OpenAiProviderError::Url)?)
        .auth(Rc::new(LocalBearerAuthResolver::new(
            "OPENAI_API_KEY",
            None,
        )))
        .models(crate::openai_models()?)
        .api(pi_ai::OpenAiResponses::API_ID, responses_api)
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
