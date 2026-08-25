//! Mistral erased handler, HTTP transport decorator, auth, and registration.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{MistralConversationsDecodeContext, MistralConversationsSseDecoder, mistral_models};
use futures_timer::Delay;
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    AiError, AiErrorKind, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, AssistantStream,
    AttemptFailure, AuthError, AuthInteraction, AuthResolver, CONTEXT_SAFETY_TOKENS,
    CancellationToken, ChatApi, Context, DefaultRetryClassifier, EncodeContext,
    EnvironmentApiKeyAuth, ErasedApiFullOptions, ErasedApiHandler, ErasedApiOptionsPatch, HttpBody,
    HttpChatApi, HttpRequest, HttpResponse, HttpTransport, LocalApiExecutionContext,
    LocalAssistantStream, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalChatApi,
    LocalDefaultRetryClassifier, LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi,
    LocalHttpResponse, LocalHttpTransport, LocalProviderAuthResolver, LocalProviderRegistration,
    LocalProviderResponseStream, LocalResolveAuthRequest, LocalRetryClassifier, MessageId,
    MiddlewareError, MistralCompat, MistralConversations, MistralConversationsHandoff,
    MistralSimplePatch, OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter,
    ProviderAuthResolver, ProviderPayload, ProviderRegistration, ProviderRegistrationError,
    ProviderResponseStream, ResolveAuthRequest, ResolvedAuth, RetryClassifier, RetryDecision,
    RetryPolicy, SendBoxFuture, SimpleGenerationOptions, SimpleLoweringContext, Timestamp,
    TypedModelDescriptor, estimate_context_tokens, transform_context_for_model, trim_ecmascript,
};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_MISTRAL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4_000;

/// Mistral Conversations erased API-family handler.
#[derive(Clone, Debug)]
pub struct MistralConversationsHandler {
    api: ApiId,
}

impl Default for MistralConversationsHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(MistralConversations::API_ID),
        }
    }
}

impl ErasedApiHandler for MistralConversationsHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &pi_ai::ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(model, context, simple, patch, execution.endpoint)
    }

    fn encode_full(
        &self,
        model: &pi_ai::ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.request_options,
        )
    }

    fn decode_stream(
        &self,
        mut response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        let mut decoder = MistralConversationsSseDecoder::new(decode_context(execution.model));
        for diagnostic in std::mem::take(&mut response.diagnostics) {
            decoder.add_diagnostic(diagnostic);
        }
        let pending = decoder.take_events().into();
        AssistantStream::new(stream::unfold(
            SendDecodeState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_send_event,
        ))
    }
}

impl LocalErasedApiHandler for MistralConversationsHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &pi_ai::ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(model, context, simple, patch, execution.endpoint)
    }

    fn encode_full(
        &self,
        model: &pi_ai::ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.request_options,
        )
    }

    fn decode_stream(
        &self,
        mut response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        let mut decoder = MistralConversationsSseDecoder::new(decode_context(execution.model));
        for diagnostic in std::mem::take(&mut response.diagnostics) {
            decoder.add_diagnostic(diagnostic);
        }
        let pending = decoder.take_events().into();
        LocalAssistantStream::new(stream::unfold(
            LocalDecodeState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_local_event,
        ))
    }
}

struct SendDecodeState {
    body: HttpBody,
    decoder: MistralConversationsSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeState {
    body: LocalHttpBody,
    decoder: MistralConversationsSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, pi_ai::TransportError>>),
}

async fn next_send_event(
    mut state: SendDecodeState,
) -> Option<(pi_ai::AssistantEvent, SendDecodeState)> {
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
            BodyPoll::Body(Some(Err(mut error))) => {
                for diagnostic in std::mem::take(&mut error.diagnostics) {
                    state.decoder.add_diagnostic(diagnostic);
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport_error(error));
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
) -> Option<(pi_ai::AssistantEvent, LocalDecodeState)> {
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
            BodyPoll::Body(Some(Err(mut error))) => {
                for diagnostic in std::mem::take(&mut error.diagnostics) {
                    state.decoder.add_diagnostic(diagnostic);
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport_error(error));
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
    model: &pi_ai::ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::MistralConversations(config) = &model.api else {
        return Err(invalid_request(
            model,
            "model does not use mistral-conversations",
        ));
    };
    let typed = TypedModelDescriptor::<MistralConversations> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = MistralCompat;
    let handoff = MistralConversationsHandoff::default();
    let projected = transform_context_for_model(context, model, &Default::default(), &handoff)
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
    let options = MistralConversations::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compatibility,
            effective_base_url: endpoint,
            estimated_input_tokens: estimate.tokens,
            available_context_tokens: available,
        },
        simple,
        &patch,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let suppress_affinity = has_header_override(&model.common.headers, "x-affinity")
        || has_header_override(&simple.headers, "x-affinity");
    encode_options(
        model,
        &projected,
        endpoint,
        typed,
        &compatibility,
        &options,
        suppress_affinity,
    )
}

fn encode_full(
    model: &pi_ai::ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
    request_options: &pi_ai::ApiRequestOptions,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::MistralConversations(config) = &model.api else {
        return Err(invalid_request(
            model,
            "model does not use mistral-conversations",
        ));
    };
    let options = options
        .downcast_ref::<MistralConversations>()
        .ok_or_else(|| invalid_request(model, "invalid Mistral options type"))?;
    let typed = TypedModelDescriptor::<MistralConversations> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = MistralCompat;
    let handoff = MistralConversationsHandoff::default();
    let projected = transform_context_for_model(context, model, &Default::default(), &handoff)
        .map_err(|error| invalid_request(model, error.to_string()))?
        .context;
    let suppress_affinity = has_header_override(&model.common.headers, "x-affinity")
        || has_header_override(&request_options.headers, "x-affinity");
    encode_options(
        model,
        &projected,
        endpoint,
        typed,
        &compatibility,
        options,
        suppress_affinity,
    )
}

fn encode_options(
    model: &pi_ai::ModelDescriptor,
    context: &Context,
    endpoint: &Url,
    typed: TypedModelDescriptor<MistralConversations>,
    compatibility: &MistralCompat,
    options: &pi_ai::MistralOptions,
    suppress_affinity: bool,
) -> Result<ProviderPayload, AiError> {
    let wire = MistralConversations::encode(
        EncodeContext {
            model: &typed,
            context,
            compat: compatibility,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let session_id = (!suppress_affinity
        && options.cache_retention != Some(pi_ai::CacheRetention::None))
    .then(|| options.session_id.clone())
    .flatten()
    .filter(|session_id| !session_id.is_empty());
    Ok(
        ProviderPayload::typed::<MistralConversations, _>(Method::POST, typed, wire, |request| {
            let mut request = request.clone();
            to_mistral_wire_payload(&mut request);
            OrderedJsonWriter::to_vec(&request.into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode Mistral payload: {error}"),
                )
            })
        })
        .with_transport_session_id(session_id),
    )
}

fn has_header_override(headers: &pi_ai::HeaderMapSpec, target: &str) -> bool {
    headers.keys().any(|name| name.eq_ignore_ascii_case(target))
}

fn remap_property(object: &mut OrderedJsonObject, source: &str, target: &str) {
    if let Some(value) = object.remove(source) {
        object.insert(target, value);
    }
}

fn remap_content_chunk(value: &mut OrderedJsonValue) {
    let OrderedJsonValue::Object(chunk) = value else {
        return;
    };
    for (source, target) in [
        ("imageUrl", "image_url"),
        ("documentUrl", "document_url"),
        ("documentName", "document_name"),
        ("fileId", "file_id"),
        ("referenceIds", "reference_ids"),
        ("inputAudio", "input_audio"),
    ] {
        remap_property(chunk, source, target);
    }
}

fn remap_message(value: &mut OrderedJsonValue) {
    let OrderedJsonValue::Object(message) = value else {
        return;
    };
    remap_property(message, "toolCalls", "tool_calls");
    remap_property(message, "toolCallId", "tool_call_id");
    if let Some(OrderedJsonValue::Array(content)) = message.get_mut("content") {
        for chunk in content.as_mut_slice() {
            remap_content_chunk(chunk);
        }
    }
}

fn to_mistral_wire_payload(payload: &mut OrderedJsonObject) {
    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap_property(payload, source, target);
    }
    if let Some(OrderedJsonValue::Array(messages)) = payload.get_mut("messages") {
        for message in messages.as_mut_slice() {
            remap_message(message);
        }
    }
    if let Some(OrderedJsonValue::Object(response_format)) = payload.get_mut("response_format") {
        remap_property(response_format, "jsonSchema", "json_schema");
        if let Some(OrderedJsonValue::Object(json_schema)) = response_format.get_mut("json_schema")
        {
            remap_property(json_schema, "schemaDefinition", "schema");
        }
    }
}

fn parse_patch(
    model: &pi_ai::ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<MistralSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(MistralSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported Mistral options schema version {}",
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn decode_context(model: &pi_ai::ModelDescriptor) -> MistralConversationsDecodeContext {
    MistralConversationsDecodeContext {
        message_id: MessageId::new(format!(
            "mistral-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        timestamp: now_timestamp(),
        supports_finish_reason: true,
        grammar_tool_input_properties: Default::default(),
    }
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn invalid_request(model: &pi_ai::ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

/// Creates the Send Mistral API over an injected raw transport.
pub fn mistral_conversations_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(MistralConversationsHandler::default()),
        Arc::new(MistralTransport { inner: transport }),
    ))
}

/// Creates the local-executor Mistral API.
pub fn local_mistral_conversations_api(
    transport: Rc<dyn LocalHttpTransport>,
) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(MistralConversationsHandler::default()),
        Rc::new(LocalMistralTransport { inner: transport }),
    ))
}

#[derive(Clone)]
struct MistralTransport {
    inner: Arc<dyn HttpTransport>,
}

impl HttpTransport for MistralTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, pi_ai::TransportError>> {
        request.url = mistral_url(&request.url);
        ensure_headers(&mut request.headers, request.session_id.as_deref());
        let timeout = request.timeout.unwrap_or(DEFAULT_MISTRAL_TIMEOUT);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let started = Instant::now();
            let response = inner.execute(request, cancellation).fuse();
            let deadline = Delay::new(timeout).fuse();
            futures_util::pin_mut!(response, deadline);
            let mut response = futures_util::select_biased! {
                response = response => response?,
                _ = deadline => return Err(mistral_timeout_error(timeout)),
            };
            let remaining = timeout.saturating_sub(started.elapsed());
            response.body = send_body_with_timeout(response.body, remaining, timeout);
            Ok(response)
        })
    }
}

#[derive(Clone)]
struct LocalMistralTransport {
    inner: Rc<dyn LocalHttpTransport>,
}

impl LocalHttpTransport for LocalMistralTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, pi_ai::TransportError>> {
        request.url = mistral_url(&request.url);
        ensure_headers(&mut request.headers, request.session_id.as_deref());
        let timeout = request.timeout.unwrap_or(DEFAULT_MISTRAL_TIMEOUT);
        let inner = Rc::clone(&self.inner);
        Box::pin(async move {
            let started = Instant::now();
            let response = inner.execute(request, cancellation).fuse();
            let deadline = Delay::new(timeout).fuse();
            futures_util::pin_mut!(response, deadline);
            let mut response = futures_util::select_biased! {
                response = response => response?,
                _ = deadline => return Err(mistral_timeout_error(timeout)),
            };
            let remaining = timeout.saturating_sub(started.elapsed());
            response.body = local_body_with_timeout(response.body, remaining, timeout);
            Ok(response)
        })
    }
}

enum TimedBodyPoll {
    TimedOut,
    Body(Option<Result<Vec<u8>, pi_ai::TransportError>>),
}

fn send_body_with_timeout(body: HttpBody, remaining: Duration, total: Duration) -> HttpBody {
    Box::pin(stream::unfold(
        (body, Delay::new(remaining), false),
        move |(mut body, mut deadline, done)| async move {
            if done {
                return None;
            }
            let outcome = {
                let next = body.next().fuse();
                let expired = (&mut deadline).fuse();
                futures_util::pin_mut!(next, expired);
                futures_util::select_biased! {
                    _ = expired => TimedBodyPoll::TimedOut,
                    item = next => TimedBodyPoll::Body(item),
                }
            };
            match outcome {
                TimedBodyPoll::TimedOut => {
                    Some((Err(mistral_timeout_error(total)), (body, deadline, true)))
                }
                TimedBodyPoll::Body(Some(item)) => Some((item, (body, deadline, false))),
                TimedBodyPoll::Body(None) => None,
            }
        },
    ))
}

fn local_body_with_timeout(
    body: LocalHttpBody,
    remaining: Duration,
    total: Duration,
) -> LocalHttpBody {
    Box::pin(stream::unfold(
        (body, Delay::new(remaining), false),
        move |(mut body, mut deadline, done)| async move {
            if done {
                return None;
            }
            let outcome = {
                let next = body.next().fuse();
                let expired = (&mut deadline).fuse();
                futures_util::pin_mut!(next, expired);
                futures_util::select_biased! {
                    _ = expired => TimedBodyPoll::TimedOut,
                    item = next => TimedBodyPoll::Body(item),
                }
            };
            match outcome {
                TimedBodyPoll::TimedOut => {
                    Some((Err(mistral_timeout_error(total)), (body, deadline, true)))
                }
                TimedBodyPoll::Body(Some(item)) => Some((item, (body, deadline, false))),
                TimedBodyPoll::Body(None) => None,
            }
        },
    ))
}

fn mistral_timeout_error(timeout: Duration) -> pi_ai::TransportError {
    pi_ai::TransportError::new(
        "timeout",
        format!("Mistral request timed out after {}ms", timeout.as_millis()),
    )
}

fn mistral_url(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/v1/chat/completions"));
    url
}

fn ensure_headers(headers: &mut HeaderMap, session_id: Option<&str>) {
    headers
        .entry(header::ACCEPT)
        .or_insert(HeaderValue::from_static("text/event-stream"));
    headers
        .entry(header::CONTENT_TYPE)
        .or_insert(HeaderValue::from_static("application/json"));
    headers
        .entry(header::USER_AGENT)
        .or_insert(HeaderValue::from_static(concat!(
            "pi-ai-rs/",
            env!("CARGO_PKG_VERSION")
        )));
    if !headers.contains_key("x-affinity")
        && let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty())
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        headers.insert("x-affinity", value);
    }
}

/// Mistral retry classification retaining Pi's provider error response text
/// after the terminal retry decision has been made.
#[derive(Default)]
pub struct MistralRetryClassifier {
    inner: DefaultRetryClassifier,
}

impl RetryClassifier for MistralRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        self.inner.classify(failure, policy)
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        normalize_mistral_terminal_failure(failure)
    }
}

/// Local-executor counterpart to [`MistralRetryClassifier`].
#[derive(Default)]
pub struct LocalMistralRetryClassifier {
    inner: LocalDefaultRetryClassifier,
}

impl LocalRetryClassifier for LocalMistralRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        self.inner.classify(failure, policy)
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        normalize_mistral_terminal_failure(failure)
    }
}

fn normalize_mistral_terminal_failure(failure: AttemptFailure) -> AttemptFailure {
    match failure {
        AttemptFailure::Http {
            attempt,
            status,
            message,
            ..
        } => {
            let body = trim_ecmascript(&message);
            let detail = if body.is_empty() {
                http::StatusCode::from_u16(status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .unwrap_or("Request failed")
                    .to_owned()
            } else {
                truncate_mistral_error_text(body, MAX_MISTRAL_ERROR_BODY_CHARS)
            };
            AttemptFailure::Transport {
                attempt,
                source: Box::new(
                    pi_ai::TransportError::new(
                        "mistral_api_error",
                        format!("Mistral API error ({status}): {detail}"),
                    )
                    .with_status(status),
                ),
            }
        }
        other => other,
    }
}

fn truncate_mistral_error_text(text: &str, max_chars: usize) -> String {
    let utf16_len = text.encode_utf16().count();
    if utf16_len <= max_chars {
        return text.to_owned();
    }

    let mut prefix = String::new();
    let mut prefix_len = 0;
    for character in text.chars() {
        let character_len = character.len_utf16();
        if prefix_len + character_len > max_chars {
            break;
        }
        prefix.push(character);
        prefix_len += character_len;
    }
    format!(
        "{prefix}... [truncated {} chars]",
        utf16_len.saturating_sub(max_chars)
    )
}

struct MistralAuth {
    inner: ProviderAuthResolver,
}

impl AuthResolver for MistralAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_bearer(&mut auth)?;
            Ok(Some(auth))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalMistralAuth {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalMistralAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_bearer(&mut auth)?;
            Ok(Some(auth))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

fn insert_bearer(auth: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(key) = auth.api_key.take() else {
        return Ok(());
    };
    let value = HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
        .map_err(|_| AuthError::new("invalid_api_key", "API key cannot be encoded as a header"))?;
    auth.headers.insert(header::AUTHORIZATION, value);
    Ok(())
}

/// Failure while building the Mistral provider.
#[derive(Debug)]
pub enum MistralProviderError {
    /// Invalid pinned catalog.
    Catalog(crate::MistralCatalogError),
    /// Invalid endpoint URL.
    Url(url::ParseError),
    /// Invalid registration composition.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for MistralProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
            Self::Url(error) => write!(formatter, "URL error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for MistralProviderError {}

/// Builds the complete Send Mistral provider registration.
pub fn mistral_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, MistralProviderError> {
    ProviderRegistration::builder("mistral")
        .display_name("Mistral")
        .base_url(Url::parse("https://api.mistral.ai").map_err(MistralProviderError::Url)?)
        .auth(Arc::new(MistralAuth {
            inner: ProviderAuthResolver::new(
                Some(Arc::new(EnvironmentApiKeyAuth::new(
                    "Mistral API key",
                    ["MISTRAL_API_KEY"],
                ))),
                None,
            ),
        }))
        .models(mistral_models().map_err(MistralProviderError::Catalog)?)
        .api(
            MistralConversations::API_ID,
            mistral_conversations_api(transport),
        )
        .retry_classifier(Arc::new(MistralRetryClassifier::default()))
        .build()
        .map_err(MistralProviderError::Registration)
}

/// Builds the complete local-executor Mistral provider registration.
pub fn local_mistral_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, MistralProviderError> {
    LocalProviderRegistration::builder("mistral")
        .display_name("Mistral")
        .base_url(Url::parse("https://api.mistral.ai").map_err(MistralProviderError::Url)?)
        .auth(Rc::new(LocalMistralAuth {
            inner: LocalProviderAuthResolver::new(
                Some(Rc::new(EnvironmentApiKeyAuth::new(
                    "Mistral API key",
                    ["MISTRAL_API_KEY"],
                ))),
                None,
            ),
        }))
        .models(mistral_models().map_err(MistralProviderError::Catalog)?)
        .api(
            MistralConversations::API_ID,
            local_mistral_conversations_api(transport),
        )
        .retry_classifier(Rc::new(LocalMistralRetryClassifier::default()))
        .build()
        .map_err(MistralProviderError::Registration)
}
