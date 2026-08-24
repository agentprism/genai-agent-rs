//! OpenAI Responses family handlers and HTTP/SSE transport decorators.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{OpenAiResponsesDecodeContext, OpenAiResponsesSseDecoder};
use futures_util::{FutureExt, StreamExt, future, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION, AiError, AiErrorKind, ApiCallOptions,
    ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, ApiRequestOptions,
    AssistantMessageDiagnostic, AssistantStream, CONTEXT_SAFETY_TOKENS, CancellationToken, ChatApi,
    Context, DiagnosticErrorCode, DiagnosticErrorInfo, EncodeContext, ErasedApiFullOptions,
    ErasedApiHandler, ErasedApiOptionsPatch, HttpBody, HttpChatApi, HttpRequest, HttpResponse,
    HttpTransport, LocalApiExecutionContext, LocalAssistantStream, LocalBoxFuture, LocalChatApi,
    LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalProviderResponseStream, MessageId, MiddlewareError, ModelDescriptor, OpenAiCodexResponses,
    OpenAiCodexResponsesOptions, OpenAiCodexResponsesSimplePatch, OpenAiResponses,
    OpenAiResponsesHandoff, OpenAiResponsesOptions, OpenAiResponsesSimplePatch, OrderedJsonObject,
    OrderedJsonValue, OrderedJsonWriter, ProviderPayload, ProviderResponseStream, SendBoxFuture,
    SimpleGenerationOptions, SimpleLoweringContext, StreamTransport, Timestamp,
    TypedModelDescriptor, apply_openai_codex_responses_full_headers,
    apply_openai_responses_full_headers, estimate_context_tokens,
    openai_codex_responses_transport_session_id, responses_grammar_tool_input_properties,
    transform_context_for_model,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_RESPONSES_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CODEX_WEBSOCKET_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
const PI_AI_RUST_USER_AGENT: &str = concat!("pi-ai-rs/", env!("CARGO_PKG_VERSION"));
const CODEX_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const CODEX_WEBSOCKET_CONNECTION_LIMIT: &str = "websocket_connection_limit_reached";
const CODEX_PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";

/// Erased handler for the public OpenAI Responses API.
#[derive(Clone, Debug)]
pub struct OpenAiResponsesHandler {
    api: ApiId,
}

impl Default for OpenAiResponsesHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(OpenAiResponses::API_ID),
        }
    }
}

/// Erased handler for ChatGPT's Codex Responses API.
#[derive(Clone, Debug)]
pub struct OpenAiCodexResponsesHandler {
    api: ApiId,
}

impl Default for OpenAiCodexResponsesHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(OpenAiCodexResponses::API_ID),
        }
    }
}

macro_rules! impl_send_handler {
    ($handler:ty, $lower:ident, $full:ident, $headers:ident) => {
        impl ErasedApiHandler for $handler {
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
                $lower(model, context, simple, patch, execution.endpoint)
            }

            fn encode_full(
                &self,
                model: &ModelDescriptor,
                context: &Context,
                options: &ErasedApiFullOptions,
                execution: &ApiExecutionContext<'_>,
            ) -> Result<ProviderPayload, AiError> {
                $full(model, context, options, execution.endpoint)
            }

            fn apply_full_options_headers(
                &self,
                model: &ModelDescriptor,
                context: &Context,
                options: &ErasedApiFullOptions,
                effective_base_url: &Url,
                request_options: &ApiRequestOptions,
                headers: &mut HeaderMap,
            ) -> Result<(), AiError> {
                $headers(
                    model,
                    context,
                    options,
                    effective_base_url,
                    request_options,
                    headers,
                )
            }

            fn decode_stream(
                &self,
                mut response: ProviderResponseStream,
                execution: &ApiExecutionContext<'_>,
            ) -> AssistantStream {
                let mut decoder = OpenAiResponsesSseDecoder::new(responses_decode_context(
                    execution.model,
                    execution.context,
                    execution.endpoint,
                    execution.call_options,
                ));
                for diagnostic in std::mem::take(&mut response.diagnostics) {
                    decoder
                        .add_diagnostic(diagnostic)
                        .expect("transport diagnostics follow MessageStarted");
                }
                let pending = decoder.take_events().into();
                AssistantStream::new(stream::unfold(
                    SendResponsesDecodeState {
                        body: response.body,
                        decoder,
                        cancellation: execution.cancellation.clone(),
                        pending,
                        done: false,
                    },
                    next_send_responses_event,
                ))
            }
        }
    };
}

macro_rules! impl_local_handler {
    ($handler:ty, $lower:ident, $full:ident, $headers:ident) => {
        impl LocalErasedApiHandler for $handler {
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
                $lower(model, context, simple, patch, execution.endpoint)
            }

            fn encode_full(
                &self,
                model: &ModelDescriptor,
                context: &Context,
                options: &ErasedApiFullOptions,
                execution: &LocalApiExecutionContext<'_>,
            ) -> Result<ProviderPayload, AiError> {
                $full(model, context, options, execution.endpoint)
            }

            fn apply_full_options_headers(
                &self,
                model: &ModelDescriptor,
                context: &Context,
                options: &ErasedApiFullOptions,
                effective_base_url: &Url,
                request_options: &ApiRequestOptions,
                headers: &mut HeaderMap,
            ) -> Result<(), AiError> {
                $headers(
                    model,
                    context,
                    options,
                    effective_base_url,
                    request_options,
                    headers,
                )
            }

            fn decode_stream(
                &self,
                mut response: LocalProviderResponseStream,
                execution: &LocalApiExecutionContext<'_>,
            ) -> LocalAssistantStream {
                let mut decoder = OpenAiResponsesSseDecoder::new(responses_decode_context(
                    execution.model,
                    execution.context,
                    execution.endpoint,
                    execution.call_options,
                ));
                for diagnostic in std::mem::take(&mut response.diagnostics) {
                    decoder
                        .add_diagnostic(diagnostic)
                        .expect("transport diagnostics follow MessageStarted");
                }
                let pending = decoder.take_events().into();
                LocalAssistantStream::new(stream::unfold(
                    LocalResponsesDecodeState {
                        body: response.body,
                        decoder,
                        cancellation: execution.cancellation.clone(),
                        pending,
                        done: false,
                    },
                    next_local_responses_event,
                ))
            }
        }
    };
}

impl_send_handler!(
    OpenAiResponsesHandler,
    lower_openai_responses,
    encode_openai_responses_full,
    apply_openai_responses_full_option_headers
);
impl_local_handler!(
    OpenAiResponsesHandler,
    lower_openai_responses,
    encode_openai_responses_full,
    apply_openai_responses_full_option_headers
);
impl_send_handler!(
    OpenAiCodexResponsesHandler,
    lower_openai_codex_responses,
    encode_openai_codex_responses_full,
    apply_openai_codex_responses_full_option_headers
);
impl_local_handler!(
    OpenAiCodexResponsesHandler,
    lower_openai_codex_responses,
    encode_openai_codex_responses_full,
    apply_openai_codex_responses_full_option_headers
);

struct SendResponsesDecodeState {
    body: HttpBody,
    decoder: OpenAiResponsesSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

struct LocalResponsesDecodeState {
    body: LocalHttpBody,
    decoder: OpenAiResponsesSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<pi_ai::AssistantEvent>,
    done: bool,
}

enum ResponsesBodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, pi_ai::TransportError>>),
}

async fn next_send_responses_event(
    mut state: SendResponsesDecodeState,
) -> Option<(pi_ai::AssistantEvent, SendResponsesDecodeState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_send_body(&mut state.body, &state.cancellation).await {
            ResponsesBodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            ResponsesBodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            ResponsesBodyPoll::Body(Some(Err(error))) => {
                for diagnostic in error.diagnostics {
                    state
                        .decoder
                        .add_diagnostic(diagnostic)
                        .expect("body diagnostics precede terminal failure");
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            ResponsesBodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_local_responses_event(
    mut state: LocalResponsesDecodeState,
) -> Option<(pi_ai::AssistantEvent, LocalResponsesDecodeState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_local_body(&mut state.body, &state.cancellation).await {
            ResponsesBodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            ResponsesBodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            ResponsesBodyPoll::Body(Some(Err(error))) => {
                for diagnostic in error.diagnostics {
                    state
                        .decoder
                        .add_diagnostic(diagnostic)
                        .expect("body diagnostics precede terminal failure");
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            ResponsesBodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_send_body(
    body: &mut HttpBody,
    cancellation: &CancellationToken,
) -> ResponsesBodyPoll {
    if cancellation.is_cancelled() {
        return ResponsesBodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => ResponsesBodyPoll::Cancelled,
        item = next => ResponsesBodyPoll::Body(item),
    }
}

async fn next_local_body(
    body: &mut LocalHttpBody,
    cancellation: &CancellationToken,
) -> ResponsesBodyPoll {
    if cancellation.is_cancelled() {
        return ResponsesBodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => ResponsesBodyPoll::Cancelled,
        item = next => ResponsesBodyPoll::Body(item),
    }
}

fn lower_openai_responses(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::OpenAiResponses(config) = &model.api else {
        return Err(wrong_family(model, OpenAiResponses::API_ID));
    };
    let typed = TypedModelDescriptor::<OpenAiResponses> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compat = OpenAiResponses::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let projected = projected_context(model, context)?;
    let estimate = estimate_context_tokens(&projected)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let available = available_context(model, estimate.tokens);
    let patch = parse_patch::<OpenAiResponsesSimplePatch>(model, patch, OpenAiResponses::API_ID)?;
    let options = OpenAiResponses::lower_simple(
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
    let wire = OpenAiResponses::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: endpoint,
        },
        &options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    payload::<OpenAiResponses>(typed, wire)
}

fn lower_openai_codex_responses(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::OpenAiCodexResponses(config) = &model.api else {
        return Err(wrong_family(model, OpenAiCodexResponses::API_ID));
    };
    let typed = TypedModelDescriptor::<OpenAiCodexResponses> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compat = OpenAiCodexResponses::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let projected = projected_context(model, context)?;
    let estimate = estimate_context_tokens(&projected)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let available = available_context(model, estimate.tokens);
    let patch =
        parse_patch::<OpenAiCodexResponsesSimplePatch>(model, patch, OpenAiCodexResponses::API_ID)?;
    let options = OpenAiCodexResponses::lower_simple(
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
    let wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: endpoint,
        },
        &options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let session_id = openai_codex_responses_transport_session_id(
        options.cache_retention,
        options.session_id.as_deref(),
    );
    payload::<OpenAiCodexResponses>(typed, wire)
        .map(|payload| payload.with_transport_session_id(session_id))
}

fn encode_openai_responses_full(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::OpenAiResponses(config) = &model.api else {
        return Err(wrong_family(model, OpenAiResponses::API_ID));
    };
    let options = options
        .downcast_ref::<OpenAiResponses>()
        .ok_or_else(|| invalid_request(model, "invalid openai-responses full options type"))?;
    let typed = TypedModelDescriptor::<OpenAiResponses> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compat = OpenAiResponses::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let projected = projected_context(model, context)?;
    let wire = OpenAiResponses::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    payload::<OpenAiResponses>(typed, wire)
}

fn encode_openai_codex_responses_full(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::OpenAiCodexResponses(config) = &model.api else {
        return Err(wrong_family(model, OpenAiCodexResponses::API_ID));
    };
    let options = options
        .downcast_ref::<OpenAiCodexResponses>()
        .ok_or_else(|| {
            invalid_request(model, "invalid openai-codex-responses full options type")
        })?;
    let typed = TypedModelDescriptor::<OpenAiCodexResponses> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compat = OpenAiCodexResponses::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let projected = projected_context(model, context)?;
    let wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let session_id = openai_codex_responses_transport_session_id(
        options.cache_retention,
        options.session_id.as_deref(),
    );
    payload::<OpenAiCodexResponses>(typed, wire)
        .map(|payload| payload.with_transport_session_id(session_id))
}

fn apply_openai_responses_full_option_headers(
    model: &ModelDescriptor,
    _context: &Context,
    options: &ErasedApiFullOptions,
    effective_base_url: &Url,
    _request_options: &ApiRequestOptions,
    headers: &mut HeaderMap,
) -> Result<(), AiError> {
    let ApiModelConfig::OpenAiResponses(config) = &model.api else {
        return Err(wrong_family(model, OpenAiResponses::API_ID));
    };
    let options: &OpenAiResponsesOptions = options
        .downcast_ref::<OpenAiResponses>()
        .ok_or_else(|| invalid_request(model, "invalid openai-responses full options type"))?;
    let compat = OpenAiResponses::resolve_compat(effective_base_url, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    apply_openai_responses_full_headers(&compat, options, headers)
        .map_err(|error| invalid_request(model, error.message))
}

fn apply_openai_codex_responses_full_option_headers(
    model: &ModelDescriptor,
    _context: &Context,
    options: &ErasedApiFullOptions,
    _effective_base_url: &Url,
    _request_options: &ApiRequestOptions,
    headers: &mut HeaderMap,
) -> Result<(), AiError> {
    if !matches!(model.api, ApiModelConfig::OpenAiCodexResponses(_)) {
        return Err(wrong_family(model, OpenAiCodexResponses::API_ID));
    }
    let options: &OpenAiCodexResponsesOptions = options
        .downcast_ref::<OpenAiCodexResponses>()
        .ok_or_else(|| {
            invalid_request(model, "invalid openai-codex-responses full options type")
        })?;
    apply_openai_codex_responses_full_headers(options, headers)
        .map_err(|error| invalid_request(model, error.message))
}

fn projected_context(model: &ModelDescriptor, context: &Context) -> Result<Context, AiError> {
    transform_context_for_model(context, model, &Default::default(), &OpenAiResponsesHandoff)
        .map(|result| result.context)
        .map_err(|error| invalid_request(model, error.to_string()))
}

fn available_context(model: &ModelDescriptor, estimate: u64) -> u64 {
    model
        .common
        .limits
        .context_window
        .saturating_sub(estimate)
        .saturating_sub(CONTEXT_SAFETY_TOKENS)
}

fn parse_patch<T: serde::de::DeserializeOwned + Default>(
    model: &ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
    api: &str,
) -> Result<T, AiError> {
    let Some(patch) = patch else {
        return Ok(T::default());
    };
    if patch.schema_version != 1 || patch.api.as_str() != api {
        return Err(invalid_request(
            model,
            format!("unsupported {api} options patch"),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn payload<A: ApiFamily<WireRequest = pi_ai::OrderedJsonObject>>(
    model: TypedModelDescriptor<A>,
    wire: pi_ai::OrderedJsonObject,
) -> Result<ProviderPayload, AiError> {
    Ok(ProviderPayload::typed::<A, _>(
        Method::POST,
        model,
        wire,
        |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode OpenAI Responses payload: {error}"),
                )
            })
        },
    ))
}

fn wrong_family(model: &ModelDescriptor, expected: &str) -> AiError {
    invalid_request(
        model,
        format!("model uses API {}, not {expected}", model.api.api_id()),
    )
}

fn invalid_request(model: &ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

fn responses_decode_context(
    model: &ModelDescriptor,
    context: &Context,
    endpoint: &Url,
    call_options: ApiCallOptions<'_>,
) -> OpenAiResponsesDecodeContext {
    let (api, compat) = match &model.api {
        ApiModelConfig::OpenAiResponses(config) => (
            ApiId::new(OpenAiResponses::API_ID),
            OpenAiResponses::resolve_compat(endpoint, &config.compat)
                .expect("compat resolved during lowering"),
        ),
        ApiModelConfig::OpenAiCodexResponses(config) => (
            ApiId::new(OpenAiCodexResponses::API_ID),
            OpenAiCodexResponses::resolve_compat(endpoint, &config.compat)
                .expect("compat resolved during lowering"),
        ),
        _ => (model.api.api_id(), Default::default()),
    };
    OpenAiResponsesDecodeContext {
        message_id: MessageId::new(format!(
            "openai-responses-message-{}",
            NEXT_RESPONSES_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        api,
        requested_model: model.common.model_ref.model.clone(),
        timestamp: now_timestamp(),
        grammar_tool_input_properties: responses_grammar_tool_input_properties(context, &compat)
            .expect("grammar tools validated during encoding"),
        pricing: model.common.pricing.clone(),
        requested_service_tier: requested_service_tier(model, call_options),
    }
}

fn requested_service_tier(
    model: &ModelDescriptor,
    call_options: ApiCallOptions<'_>,
) -> Option<String> {
    match (&model.api, call_options) {
        (ApiModelConfig::OpenAiResponses(_), ApiCallOptions::Simple(simple)) => simple
            .api_options
            .as_ref()
            .map(|patch| {
                serde_json::from_str::<OpenAiResponsesSimplePatch>(patch.value.get())
                    .expect("Responses simple patch was validated during lowering")
            })
            .and_then(|patch| patch.service_tier),
        (ApiModelConfig::OpenAiCodexResponses(_), ApiCallOptions::Simple(simple)) => simple
            .api_options
            .as_ref()
            .map(|patch| {
                serde_json::from_str::<OpenAiCodexResponsesSimplePatch>(patch.value.get())
                    .expect("Codex Responses simple patch was validated during lowering")
            })
            .and_then(|patch| patch.service_tier),
        (ApiModelConfig::OpenAiResponses(_), ApiCallOptions::Full(options)) => options
            .downcast_ref::<OpenAiResponses>()
            .and_then(|options| options.service_tier.clone()),
        (ApiModelConfig::OpenAiCodexResponses(_), ApiCallOptions::Full(options)) => options
            .downcast_ref::<OpenAiCodexResponses>()
            .and_then(|options| options.service_tier.clone()),
        _ => None,
    }
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

/// One already-authenticated ChatGPT Codex WebSocket exchange.
///
/// `body` is the exact UTF-8 `response.create` frame. Each item returned by
/// the transport body is one complete UTF-8 JSON event frame. Connection
/// reuse is keyed by the optional session and account IDs so native, browser,
/// and FFI hosts can provide their own WebSocket implementation without
/// coupling this crate to an executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCodexWebSocketRequest {
    /// Resolved `ws:`/`wss:` Codex endpoint.
    pub url: Url,
    /// Final WebSocket handshake headers.
    pub headers: HeaderMap,
    /// Exact `response.create` frame bytes.
    pub body: Vec<u8>,
    /// Optional session cache key. `None` requests a one-shot connection.
    pub session_id: Option<String>,
    /// Authenticated ChatGPT account ID.
    pub account_id: String,
    /// Connection/open timeout.
    pub connect_timeout: Option<std::time::Duration>,
    /// Post-connect stream idle timeout.
    pub idle_timeout: Option<std::time::Duration>,
    use_cached_context: bool,
}

impl OpenAiCodexWebSocketRequest {
    /// Returns the exact frame to send on an acquired connection.
    ///
    /// A transport must call this only after it has selected the concrete
    /// cached or newly-created socket and must associate the supplied scope
    /// with that socket for precisely the socket's cache lifetime.
    pub fn body_for_connection(
        &self,
        connection: &OpenAiCodexWebSocketConnection,
    ) -> Result<Vec<u8>, pi_ai::TransportError> {
        if !self.use_cached_context {
            return Ok(self.body.clone());
        }
        let mut full_request = codex_request_json(&self.body)?;
        let Some(full_request_object) = full_request.as_object_mut() else {
            return Err(pi_ai::TransportError::new(
                "openai_codex_request_json",
                "Codex WebSocket frame must be a JSON object",
            ));
        };
        full_request_object.remove("type");
        let continuation = connection.continuation();
        codex_response_create_frame(codex_cached_logical_request(
            &full_request,
            continuation.as_ref(),
        ))
    }
}

/// Opaque state whose lifetime is exactly one cached WebSocket connection.
///
/// Hosts create one scope per physical socket and reuse that scope only while
/// reusing the same socket. Dropping the socket cache entry drops its
/// continuation without a provider-global timer or dormant session map.
#[derive(Clone, Default)]
pub struct OpenAiCodexWebSocketConnection {
    continuation: Arc<Mutex<Option<CodexContinuation>>>,
}

impl OpenAiCodexWebSocketConnection {
    /// Creates empty state for one newly-created physical connection.
    pub fn new() -> Self {
        Self::default()
    }

    fn continuation(&self) -> Option<CodexContinuation> {
        self.continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn replace_continuation(&self, continuation: Option<CodexContinuation>) {
        *self
            .continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = continuation;
    }
}

impl fmt::Debug for OpenAiCodexWebSocketConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCodexWebSocketConnection")
            .field("continuation", &"<opaque connection state>")
            .finish()
    }
}

/// Established Send WebSocket exchange and its concrete connection scope.
pub struct OpenAiCodexWebSocketResponse {
    /// State bound to the physical connection selected by the transport.
    pub connection: OpenAiCodexWebSocketConnection,
    /// Raw JSON event frames from the exchange.
    pub body: HttpBody,
}

/// Established local WebSocket exchange and its concrete connection scope.
pub struct LocalOpenAiCodexWebSocketResponse {
    /// State bound to the physical connection selected by the transport.
    pub connection: OpenAiCodexWebSocketConnection,
    /// Raw JSON event frames from the exchange.
    pub body: LocalHttpBody,
}

/// Send-capable injected Codex WebSocket exchange capability.
pub trait OpenAiCodexWebSocketTransport: Send + Sync + 'static {
    /// Acquires a socket, selects the frame with
    /// [`OpenAiCodexWebSocketRequest::body_for_connection`], sends it, and
    /// returns the same socket's connection scope with raw JSON event frames.
    fn execute(
        &self,
        request: OpenAiCodexWebSocketRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OpenAiCodexWebSocketResponse, pi_ai::TransportError>>;
}

/// Local-executor injected Codex WebSocket exchange capability.
pub trait LocalOpenAiCodexWebSocketTransport: 'static {
    /// Local counterpart to [`OpenAiCodexWebSocketTransport::execute`].
    fn execute(
        &self,
        request: OpenAiCodexWebSocketRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalOpenAiCodexWebSocketResponse, pi_ai::TransportError>>;
}

/// Creates the Send OpenAI Responses API.
pub fn openai_responses_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(OpenAiResponsesHandler::default()),
        Arc::new(OpenAiResponsesTransport::new(transport)),
    ))
}

/// Creates the local OpenAI Responses API.
pub fn local_openai_responses_api(transport: Rc<dyn LocalHttpTransport>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(OpenAiResponsesHandler::default()),
        Rc::new(LocalOpenAiResponsesTransport::new(transport)),
    ))
}

/// Creates the Send OpenAI Codex Responses SSE API.
pub fn openai_codex_responses_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(OpenAiCodexResponsesHandler::default()),
        Arc::new(OpenAiCodexResponsesTransport::new(transport)),
    ))
}

/// Creates the complete Send OpenAI Codex Responses API with selectable SSE,
/// WebSocket, cached-WebSocket, and automatic-fallback modes.
pub fn openai_codex_responses_api_with_websocket(
    transport: Arc<dyn HttpTransport>,
    websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(OpenAiCodexResponsesHandler::default()),
        Arc::new(OpenAiCodexResponsesTransport::with_websocket(
            transport, websocket,
        )),
    ))
}

/// Creates the local OpenAI Codex Responses SSE API.
pub fn local_openai_codex_responses_api(
    transport: Rc<dyn LocalHttpTransport>,
) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(OpenAiCodexResponsesHandler::default()),
        Rc::new(LocalOpenAiCodexResponsesTransport::new(transport)),
    ))
}

/// Creates the complete local OpenAI Codex Responses API with selectable
/// transports and automatic pre-stream SSE fallback.
pub fn local_openai_codex_responses_api_with_websocket(
    transport: Rc<dyn LocalHttpTransport>,
    websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(OpenAiCodexResponsesHandler::default()),
        Rc::new(LocalOpenAiCodexResponsesTransport::with_websocket(
            transport, websocket,
        )),
    ))
}

/// Transport decorator resolving a base URL to `/responses`.
#[derive(Clone)]
pub struct OpenAiResponsesTransport {
    inner: Arc<dyn HttpTransport>,
}

impl OpenAiResponsesTransport {
    /// Wraps an injected transport.
    pub fn new(inner: Arc<dyn HttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for OpenAiResponsesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for OpenAiResponsesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, pi_ai::TransportError>> {
        request.url = responses_url(&request.url);
        ensure_responses_headers(&mut request.headers, false);
        self.inner.execute(request, cancellation)
    }
}

/// Local counterpart to [`OpenAiResponsesTransport`].
#[derive(Clone)]
pub struct LocalOpenAiResponsesTransport {
    inner: Rc<dyn LocalHttpTransport>,
}

impl LocalOpenAiResponsesTransport {
    /// Wraps an injected local transport.
    pub fn new(inner: Rc<dyn LocalHttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for LocalOpenAiResponsesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalOpenAiResponsesTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalOpenAiResponsesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, pi_ai::TransportError>> {
        request.url = responses_url(&request.url);
        ensure_responses_headers(&mut request.headers, false);
        self.inner.execute(request, cancellation)
    }
}

/// Codex SSE decorator resolving the ChatGPT backend endpoint and reasserting
/// the provider-mandatory transport headers after logical header overlays.
#[derive(Clone)]
pub struct OpenAiCodexResponsesTransport {
    inner: Arc<dyn HttpTransport>,
    websocket: Option<Arc<dyn OpenAiCodexWebSocketTransport>>,
    websocket_state: Arc<Mutex<CodexWebSocketState>>,
}

impl OpenAiCodexResponsesTransport {
    /// Wraps an injected transport.
    pub fn new(inner: Arc<dyn HttpTransport>) -> Self {
        Self {
            inner,
            websocket: None,
            websocket_state: Arc::new(Mutex::new(CodexWebSocketState::default())),
        }
    }

    /// Wraps injected HTTP and WebSocket capabilities.
    pub fn with_websocket(
        inner: Arc<dyn HttpTransport>,
        websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
    ) -> Self {
        Self {
            inner,
            websocket: Some(websocket),
            websocket_state: Arc::new(Mutex::new(CodexWebSocketState::default())),
        }
    }
}

impl fmt::Debug for OpenAiCodexResponsesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCodexResponsesTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for OpenAiCodexResponsesTransport {
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, pi_ai::TransportError>> {
        Box::pin(self.execute_selected(request, cancellation))
    }
}

impl OpenAiCodexResponsesTransport {
    async fn execute_selected(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> Result<HttpResponse, pi_ai::TransportError> {
        let selection = request.transport.unwrap_or_default();
        // A pre-stream WebSocket failure falls through to SSE inside attempt
        // zero. If that SSE response is itself retryable, every later attempt
        // belongs to the same logical request and must remain on SSE even when
        // no session ID exists to key the cross-request fallback cache.
        if selection == StreamTransport::Sse || request.attempt > 0 {
            return self.execute_sse(request, cancellation).await;
        }

        let session_id = codex_session_id(&request);
        let fallback_active = session_id.as_ref().is_some_and(|session_id| {
            lock_codex_state(&self.websocket_state)
                .sse_fallback_sessions
                .contains(session_id)
        });
        if fallback_active {
            return self.execute_sse(request, cancellation).await;
        }

        let Some(websocket) = self.websocket.as_ref() else {
            record_codex_sse_fallback(&self.websocket_state, session_id.as_deref());
            let diagnostic = codex_fallback_diagnostic(
                selection,
                &request,
                &pi_ai::TransportError::new(
                    "openai_codex_websocket_unavailable",
                    "WebSocket transport is not available in this runtime",
                ),
            );
            return match self.execute_sse(request, cancellation).await {
                Ok(mut response) => {
                    response.diagnostics.push(diagnostic);
                    Ok(response)
                }
                Err(error) => Err(error.with_diagnostic(diagnostic)),
            };
        };
        let cached = matches!(
            selection,
            StreamTransport::Auto | StreamTransport::WebsocketCached
        );
        match execute_codex_websocket_send(
            websocket.as_ref(),
            &self.websocket_state,
            request.clone(),
            cached,
            cancellation.clone(),
        )
        .await
        {
            Ok(response) => Ok(response),
            Err(error) if cancellation.is_cancelled() => Err(error),
            Err(error) => {
                let diagnostic = codex_fallback_diagnostic(selection, &request, &error);
                record_codex_sse_fallback(&self.websocket_state, session_id.as_deref());
                match self.execute_sse(request, cancellation).await {
                    Ok(mut response) => {
                        response.diagnostics.push(diagnostic);
                        Ok(response)
                    }
                    Err(error) => Err(error.with_diagnostic(diagnostic)),
                }
            }
        }
    }

    async fn execute_sse(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> Result<HttpResponse, pi_ai::TransportError> {
        prepare_codex_sse_request(&mut request);
        self.inner.execute(request, cancellation).await
    }
}

/// Local counterpart to [`OpenAiCodexResponsesTransport`].
#[derive(Clone)]
pub struct LocalOpenAiCodexResponsesTransport {
    inner: Rc<dyn LocalHttpTransport>,
    websocket: Option<Rc<dyn LocalOpenAiCodexWebSocketTransport>>,
    websocket_state: Rc<RefCell<CodexWebSocketState>>,
}

impl LocalOpenAiCodexResponsesTransport {
    /// Wraps an injected local transport.
    pub fn new(inner: Rc<dyn LocalHttpTransport>) -> Self {
        Self {
            inner,
            websocket: None,
            websocket_state: Rc::new(RefCell::new(CodexWebSocketState::default())),
        }
    }

    /// Wraps injected local HTTP and WebSocket capabilities.
    pub fn with_websocket(
        inner: Rc<dyn LocalHttpTransport>,
        websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
    ) -> Self {
        Self {
            inner,
            websocket: Some(websocket),
            websocket_state: Rc::new(RefCell::new(CodexWebSocketState::default())),
        }
    }
}

#[derive(Clone)]
struct CodexContinuation {
    last_request: serde_json::Value,
    last_response_id: String,
    last_response_items: Vec<serde_json::Value>,
}

#[derive(Default)]
struct CodexWebSocketState {
    sse_fallback_sessions: HashSet<String>,
}

fn lock_codex_state(
    state: &Mutex<CodexWebSocketState>,
) -> std::sync::MutexGuard<'_, CodexWebSocketState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record_codex_sse_fallback(state: &Mutex<CodexWebSocketState>, session_id: Option<&str>) {
    if let Some(session_id) = session_id {
        lock_codex_state(state)
            .sse_fallback_sessions
            .insert(session_id.to_owned());
    }
}

fn record_local_codex_sse_fallback(state: &RefCell<CodexWebSocketState>, session_id: Option<&str>) {
    if let Some(session_id) = session_id {
        state
            .borrow_mut()
            .sse_fallback_sessions
            .insert(session_id.to_owned());
    }
}

fn prepare_codex_sse_request(request: &mut HttpRequest) {
    request.url = codex_responses_url(&request.url);
    reassert_codex_headers(&mut request.headers, &request.auth_headers);
    reassert_codex_session_headers(&mut request.headers, request.session_id.as_deref());
    ensure_responses_headers(&mut request.headers, true);
    compress_codex_request(request);
}

fn codex_session_id(request: &HttpRequest) -> Option<String> {
    request.session_id.clone()
}

fn codex_account_id(request: &HttpRequest) -> Result<String, pi_ai::TransportError> {
    request
        .auth_headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            pi_ai::TransportError::new(
                "openai_codex_account",
                "OpenAI Codex credential omits chatgpt-account-id",
            )
        })
}

fn codex_websocket_url(base: &Url) -> Url {
    let mut url = codex_responses_url(base);
    match url.scheme() {
        "https" => url.set_scheme("wss").expect("wss is a valid URL scheme"),
        "http" => url.set_scheme("ws").expect("ws is a valid URL scheme"),
        _ => {}
    }
    url
}

fn prepare_codex_websocket_headers(
    headers: &mut HeaderMap,
    auth_headers: &HeaderMap,
    request_id: &str,
) {
    reassert_codex_headers(headers, auth_headers);
    headers.remove(header::ACCEPT);
    headers.remove(header::CONTENT_TYPE);
    headers.remove(header::CONTENT_ENCODING);
    headers.remove("openai-beta");
    headers.insert(
        "openai-beta",
        HeaderValue::from_static(CODEX_WEBSOCKET_BETA),
    );
    if let Ok(request_id) = HeaderValue::from_str(request_id) {
        headers.insert("x-client-request-id", request_id.clone());
        headers.insert("session-id", request_id);
    }
}

fn next_codex_websocket_request_id() -> String {
    let sequence = NEXT_CODEX_WEBSOCKET_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let timestamp = u64::try_from(timestamp_millis).unwrap_or(u64::MAX) & 0xffff_ffff_ffff;
    let random_a = (sequence >> 52) & 0x0fff;
    let random_b = sequence & 0x000f_ffff_ffff_ffff;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        timestamp >> 16,
        timestamp & 0xffff,
        0x7000 | random_a,
        0x8000 | ((random_b >> 48) & 0x3fff),
        random_b & 0xffff_ffff_ffff,
    )
}

fn codex_request_json(body: &[u8]) -> Result<serde_json::Value, pi_ai::TransportError> {
    serde_json::from_slice(body).map_err(|error| {
        pi_ai::TransportError::new(
            "openai_codex_request_json",
            format!("Codex request body is not valid JSON: {error}"),
        )
    })
}

fn codex_cached_logical_request(
    full_request: &serde_json::Value,
    continuation: Option<&CodexContinuation>,
) -> serde_json::Value {
    let Some(continuation) = continuation else {
        return full_request.clone();
    };
    let (Some(current), Some(previous)) = (
        full_request.as_object(),
        continuation.last_request.as_object(),
    ) else {
        return full_request.clone();
    };
    let mut current_without_input = current.clone();
    current_without_input.remove("input");
    current_without_input.remove("previous_response_id");
    let mut previous_without_input = previous.clone();
    previous_without_input.remove("input");
    previous_without_input.remove("previous_response_id");
    if current_without_input != previous_without_input {
        return full_request.clone();
    }
    let Some(current_input) = current.get("input").and_then(serde_json::Value::as_array) else {
        return full_request.clone();
    };
    let mut baseline = previous
        .get("input")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(continuation.last_response_items.clone());
    if current_input.len() < baseline.len() || current_input[..baseline.len()] != baseline {
        return full_request.clone();
    }
    let mut cached = current.clone();
    cached.insert(
        "input".into(),
        serde_json::Value::Array(current_input[baseline.len()..].to_vec()),
    );
    cached.insert(
        "previous_response_id".into(),
        serde_json::Value::String(continuation.last_response_id.clone()),
    );
    serde_json::Value::Object(cached)
}

fn codex_response_create_frame(
    logical_request: serde_json::Value,
) -> Result<Vec<u8>, pi_ai::TransportError> {
    let Some(logical) = logical_request.as_object() else {
        return Err(pi_ai::TransportError::new(
            "openai_codex_request_json",
            "Codex request body must be a JSON object",
        ));
    };
    let mut frame = OrderedJsonObject::new();
    frame.insert("type", "response.create");
    for (name, value) in logical {
        frame.insert(name.as_str(), OrderedJsonValue::from(value.clone()));
    }
    OrderedJsonWriter::to_vec(&frame.into()).map_err(|error| {
        pi_ai::TransportError::new(
            "openai_codex_request_json",
            format!("failed to encode Codex WebSocket frame: {error}"),
        )
    })
}

fn terminal_codex_continuation(
    frame: &[u8],
    full_request: &serde_json::Value,
    output: &mut CodexCanonicalOutput,
) -> Option<CodexContinuation> {
    let event: serde_json::Value = serde_json::from_slice(frame).ok()?;
    output.observe(&event);
    if !matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some("response.done" | "response.completed" | "response.incomplete")
    ) {
        return None;
    }
    let response = event.get("response")?.as_object()?;
    let id = response.get("id")?.as_str()?.to_owned();
    output.merge_terminal_reasoning(response.get("output").and_then(serde_json::Value::as_array));
    Some(CodexContinuation {
        last_request: full_request.clone(),
        last_response_id: id,
        last_response_items: output.items(),
    })
}

#[derive(Default)]
struct CodexCanonicalOutput {
    items: BTreeMap<u64, serde_json::Value>,
    scratch: HashMap<u64, CodexCanonicalScratch>,
    item_indices: HashMap<String, u64>,
    active_implicit_index: Option<u64>,
    next_implicit_index: u64,
}

#[derive(Default)]
struct CodexCanonicalScratch {
    added_item: Option<serde_json::Value>,
    function_arguments: String,
    custom_input: String,
}

impl CodexCanonicalOutput {
    fn observe(&mut self, event: &serde_json::Value) {
        let Some(event_type) = event.get("type").and_then(serde_json::Value::as_str) else {
            return;
        };
        match event_type {
            "response.output_item.added" => {
                let Some(item) = event.get("item") else {
                    return;
                };
                let index = self.output_index(event, true);
                self.remember_item_index(item, index);
                let scratch = self.scratch.entry(index).or_default();
                scratch.added_item = Some(item.clone());
                match item.get("type").and_then(serde_json::Value::as_str) {
                    Some("function_call") => {
                        scratch.function_arguments = item
                            .get("arguments")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                    }
                    Some("custom_tool_call") => {
                        scratch.custom_input = item
                            .get("input")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                let index = self.output_index(event, false);
                if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
                    self.scratch
                        .entry(index)
                        .or_default()
                        .function_arguments
                        .push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let index = self.output_index(event, false);
                if let Some(arguments) = event.get("arguments").and_then(serde_json::Value::as_str)
                {
                    self.scratch.entry(index).or_default().function_arguments =
                        arguments.to_owned();
                }
            }
            "response.custom_tool_call_input.delta" => {
                let index = self.output_index(event, false);
                if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str) {
                    self.scratch
                        .entry(index)
                        .or_default()
                        .custom_input
                        .push_str(delta);
                }
            }
            "response.custom_tool_call_input.done" => {
                let index = self.output_index(event, false);
                if let Some(input) = event.get("input").and_then(serde_json::Value::as_str) {
                    self.scratch.entry(index).or_default().custom_input = input.to_owned();
                }
            }
            "response.output_item.done" => {
                let Some(terminal_item) = event.get("item") else {
                    return;
                };
                let index = self.output_index_for_done(event, terminal_item);
                self.remember_item_index(terminal_item, index);
                let scratch = self.scratch.remove(&index).unwrap_or_default();
                let assembled = assemble_codex_response_item(terminal_item, scratch);
                let Some(item) = canonical_codex_response_item(&assembled) else {
                    return;
                };
                self.items.insert(index, item);
                if event.get("output_index").is_none() {
                    self.active_implicit_index = None;
                }
            }
            _ => {}
        }
    }

    fn output_index(&mut self, event: &serde_json::Value, create_implicit: bool) -> u64 {
        if let Some(index) = event
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
        {
            self.next_implicit_index = self.next_implicit_index.max(index.saturating_add(1));
            return index;
        }
        if let Some(index) = self.active_implicit_index {
            return index;
        }
        if create_implicit {
            let index = self.next_implicit_index;
            self.next_implicit_index = self.next_implicit_index.saturating_add(1);
            self.active_implicit_index = Some(index);
            return index;
        }
        self.next_implicit_index.saturating_sub(1)
    }

    fn output_index_for_done(
        &mut self,
        event: &serde_json::Value,
        item: &serde_json::Value,
    ) -> u64 {
        if let Some(index) = event
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
        {
            self.next_implicit_index = self.next_implicit_index.max(index.saturating_add(1));
            return index;
        }
        if let Some(index) = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.item_indices.get(id))
            .copied()
        {
            return index;
        }
        self.output_index(event, false)
    }

    fn remember_item_index(&mut self, item: &serde_json::Value, index: u64) {
        if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
            self.item_indices.insert(id.to_owned(), index);
        }
    }

    fn merge_terminal_reasoning(&mut self, terminal_items: Option<&Vec<serde_json::Value>>) {
        let Some(terminal_items) = terminal_items else {
            return;
        };
        for terminal in terminal_items {
            if terminal.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(encrypted) = terminal
                .get("encrypted_content")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let terminal_id = terminal.get("id").and_then(serde_json::Value::as_str);
            for item in self.items.values_mut() {
                if item.get("type").and_then(serde_json::Value::as_str) != Some("reasoning")
                    || item.get("id").and_then(serde_json::Value::as_str) != terminal_id
                    || item
                        .get("encrypted_content")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| !value.is_empty())
                {
                    continue;
                }
                if let Some(item) = item.as_object_mut() {
                    item.insert(
                        "encrypted_content".into(),
                        serde_json::Value::String(encrypted.to_owned()),
                    );
                }
            }
        }
    }

    fn items(&self) -> Vec<serde_json::Value> {
        self.items.values().cloned().collect()
    }
}

fn assemble_codex_response_item(
    terminal_item: &serde_json::Value,
    scratch: CodexCanonicalScratch,
) -> serde_json::Value {
    let mut assembled = scratch
        .added_item
        .and_then(|item| item.as_object().cloned())
        .unwrap_or_default();
    if let Some(terminal) = terminal_item.as_object() {
        for (name, value) in terminal {
            assembled.insert(name.clone(), value.clone());
        }
    }
    match terminal_item
        .get("type")
        .and_then(serde_json::Value::as_str)
    {
        Some("function_call") => {
            let arguments = terminal_item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .filter(|arguments| !arguments.is_empty())
                .unwrap_or_else(|| {
                    if scratch.function_arguments.is_empty() {
                        "{}"
                    } else {
                        &scratch.function_arguments
                    }
                });
            assembled.insert(
                "arguments".into(),
                serde_json::Value::String(arguments.to_owned()),
            );
        }
        Some("custom_tool_call") if terminal_item.get("input").is_none() => {
            assembled.insert(
                "input".into(),
                serde_json::Value::String(scratch.custom_input),
            );
        }
        _ => {}
    }
    serde_json::Value::Object(assembled)
}

fn canonical_codex_response_item(item: &serde_json::Value) -> Option<serde_json::Value> {
    let kind = item.get("type")?.as_str()?;
    let mut canonical = serde_json::Map::new();
    match kind {
        "reasoning" => return Some(item.clone()),
        "message" => {
            canonical.insert("type".into(), serde_json::Value::String("message".into()));
            canonical.insert("role".into(), serde_json::Value::String("assistant".into()));
            let text = item
                .get("content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(
                    |part| match part.get("type").and_then(serde_json::Value::as_str) {
                        Some("output_text") => part.get("text").and_then(serde_json::Value::as_str),
                        Some("refusal") => part.get("refusal").and_then(serde_json::Value::as_str),
                        _ => None,
                    },
                )
                .collect::<String>();
            canonical.insert(
                "content".into(),
                serde_json::json!([{"type":"output_text","text":text,"annotations":[]}]),
            );
            canonical.insert(
                "status".into(),
                serde_json::Value::String("completed".into()),
            );
            if let Some(id) = item.get("id").cloned() {
                canonical.insert("id".into(), id);
            }
            if let Some(phase) = item.get("phase").cloned() {
                canonical.insert("phase".into(), phase);
            }
        }
        "function_call" => {
            canonical.insert(
                "type".into(),
                serde_json::Value::String("function_call".into()),
            );
            copy_optional(&mut canonical, item, "id");
            copy_required(&mut canonical, item, "call_id")?;
            copy_required(&mut canonical, item, "name")?;
            let arguments = item
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
                .and_then(|arguments| {
                    OrderedJsonWriter::stringify(&OrderedJsonValue::from(arguments)).ok()
                })
                .or_else(|| {
                    item.get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })?;
            canonical.insert("arguments".into(), serde_json::Value::String(arguments));
            copy_optional(&mut canonical, item, "namespace");
        }
        "custom_tool_call" => {
            canonical.insert(
                "type".into(),
                serde_json::Value::String("custom_tool_call".into()),
            );
            copy_optional(&mut canonical, item, "id");
            copy_required(&mut canonical, item, "call_id")?;
            copy_required(&mut canonical, item, "name")?;
            copy_required(&mut canonical, item, "input")?;
            copy_optional(&mut canonical, item, "namespace");
        }
        _ => return None,
    }
    Some(serde_json::Value::Object(canonical))
}

fn copy_required(
    output: &mut serde_json::Map<String, serde_json::Value>,
    input: &serde_json::Value,
    name: &str,
) -> Option<()> {
    output.insert(name.into(), input.get(name)?.clone());
    Some(())
}

fn copy_optional(
    output: &mut serde_json::Map<String, serde_json::Value>,
    input: &serde_json::Value,
    name: &str,
) {
    if let Some(value) = input.get(name).cloned() {
        output.insert(name.into(), value);
    }
}

fn websocket_frame_as_sse(frame: Vec<u8>) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(frame.len() + 8);
    encoded.extend_from_slice(b"data: ");
    encoded.extend_from_slice(&frame);
    encoded.extend_from_slice(b"\n\n");
    encoded
}

fn semantic_frame_terminates_adapted_body(item: &Result<Vec<u8>, pi_ai::TransportError>) -> bool {
    let Ok(frame) = item else {
        return false;
    };
    let Some(frame) = frame
        .strip_prefix(b"data: ")
        .and_then(|frame| frame.strip_suffix(b"\n\n"))
    else {
        return false;
    };
    codex_is_terminal_event(frame) || codex_is_semantic_error_event(frame)
}

struct SendCodexContinuationGuard {
    state: Arc<Mutex<CodexWebSocketState>>,
    session_id: Option<String>,
    connection: Option<OpenAiCodexWebSocketConnection>,
    armed: bool,
}

impl SendCodexContinuationGuard {
    fn new(
        state: Arc<Mutex<CodexWebSocketState>>,
        session_id: Option<String>,
        connection: Option<OpenAiCodexWebSocketConnection>,
    ) -> Self {
        Self {
            state,
            session_id,
            connection,
            armed: true,
        }
    }

    fn clear(&self) {
        if let Some(connection) = self.connection.as_ref() {
            connection.replace_continuation(None);
        }
    }

    fn record_transport_failure(&self) {
        self.clear();
        record_codex_sse_fallback(&self.state, self.session_id.as_deref());
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SendCodexContinuationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.clear();
        }
    }
}

struct LocalCodexContinuationGuard {
    state: Rc<RefCell<CodexWebSocketState>>,
    session_id: Option<String>,
    connection: Option<OpenAiCodexWebSocketConnection>,
    armed: bool,
}

impl LocalCodexContinuationGuard {
    fn new(
        state: Rc<RefCell<CodexWebSocketState>>,
        session_id: Option<String>,
        connection: Option<OpenAiCodexWebSocketConnection>,
    ) -> Self {
        Self {
            state,
            session_id,
            connection,
            armed: true,
        }
    }

    fn clear(&self) {
        if let Some(connection) = self.connection.as_ref() {
            connection.replace_continuation(None);
        }
    }

    fn record_transport_failure(&self) {
        self.clear();
        record_local_codex_sse_fallback(&self.state, self.session_id.as_deref());
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LocalCodexContinuationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.clear();
        }
    }
}

fn adapt_send_websocket_body(
    body: HttpBody,
    state: Arc<Mutex<CodexWebSocketState>>,
    session_id: Option<String>,
    connection: Option<OpenAiCodexWebSocketConnection>,
    full_request: serde_json::Value,
    configured_transport: StreamTransport,
    request_bytes: usize,
) -> HttpBody {
    let continuation_guard =
        SendCodexContinuationGuard::new(Arc::clone(&state), session_id.clone(), connection.clone());
    Box::pin(stream::unfold(
        (
            body,
            state,
            session_id,
            connection,
            full_request,
            configured_transport,
            request_bytes,
            CodexCanonicalOutput::default(),
            continuation_guard,
            false,
        ),
        |(
            mut body,
            state,
            session_id,
            connection,
            full_request,
            configured_transport,
            request_bytes,
            mut output,
            mut continuation_guard,
            finished,
        )| async move {
            if finished {
                return None;
            }
            let item = body.next().await;
            let item = match item {
                Some(Ok(frame)) => {
                    let semantic_error = codex_is_semantic_error_event(&frame);
                    let terminal = codex_is_terminal_event(&frame);
                    if semantic_error {
                        continuation_guard.clear();
                    }
                    if let (Some(connection), Some(continuation)) = (
                        connection.as_ref(),
                        terminal_codex_continuation(&frame, &full_request, &mut output),
                    ) {
                        connection.replace_continuation(Some(continuation));
                    }
                    if terminal {
                        continuation_guard.disarm();
                    }
                    Ok(websocket_frame_as_sse(frame))
                }
                Some(Err(error)) => {
                    continuation_guard.record_transport_failure();
                    let diagnostic = codex_transport_diagnostic(
                        configured_transport,
                        &error,
                        None,
                        true,
                        "after_message_stream_start",
                        request_bytes,
                    );
                    Err(error.with_diagnostic(diagnostic))
                }
                None => {
                    let error = pi_ai::TransportError::new(
                        "openai_codex_websocket_closed_before_terminal",
                        "WebSocket stream closed before response.completed",
                    );
                    continuation_guard.record_transport_failure();
                    let diagnostic = codex_transport_diagnostic(
                        configured_transport,
                        &error,
                        None,
                        true,
                        "after_message_stream_start",
                        request_bytes,
                    );
                    Err(error.with_diagnostic(diagnostic))
                }
            };
            let finished = item.is_err() || semantic_frame_terminates_adapted_body(&item);
            Some((
                item,
                (
                    body,
                    state,
                    session_id,
                    connection,
                    full_request,
                    configured_transport,
                    request_bytes,
                    output,
                    continuation_guard,
                    finished,
                ),
            ))
        },
    ))
}

fn adapt_local_websocket_body(
    body: LocalHttpBody,
    state: Rc<RefCell<CodexWebSocketState>>,
    session_id: Option<String>,
    connection: Option<OpenAiCodexWebSocketConnection>,
    full_request: serde_json::Value,
    configured_transport: StreamTransport,
    request_bytes: usize,
) -> LocalHttpBody {
    let continuation_guard =
        LocalCodexContinuationGuard::new(Rc::clone(&state), session_id.clone(), connection.clone());
    Box::pin(stream::unfold(
        (
            body,
            state,
            session_id,
            connection,
            full_request,
            configured_transport,
            request_bytes,
            CodexCanonicalOutput::default(),
            continuation_guard,
            false,
        ),
        |(
            mut body,
            state,
            session_id,
            connection,
            full_request,
            configured_transport,
            request_bytes,
            mut output,
            mut continuation_guard,
            finished,
        )| async move {
            if finished {
                return None;
            }
            let item = body.next().await;
            let item = match item {
                Some(Ok(frame)) => {
                    let semantic_error = codex_is_semantic_error_event(&frame);
                    let terminal = codex_is_terminal_event(&frame);
                    if semantic_error {
                        continuation_guard.clear();
                    }
                    if let (Some(connection), Some(continuation)) = (
                        connection.as_ref(),
                        terminal_codex_continuation(&frame, &full_request, &mut output),
                    ) {
                        connection.replace_continuation(Some(continuation));
                    }
                    if terminal {
                        continuation_guard.disarm();
                    }
                    Ok(websocket_frame_as_sse(frame))
                }
                Some(Err(error)) => {
                    continuation_guard.record_transport_failure();
                    let diagnostic = codex_transport_diagnostic(
                        configured_transport,
                        &error,
                        None,
                        true,
                        "after_message_stream_start",
                        request_bytes,
                    );
                    Err(error.with_diagnostic(diagnostic))
                }
                None => {
                    let error = pi_ai::TransportError::new(
                        "openai_codex_websocket_closed_before_terminal",
                        "WebSocket stream closed before response.completed",
                    );
                    continuation_guard.record_transport_failure();
                    let diagnostic = codex_transport_diagnostic(
                        configured_transport,
                        &error,
                        None,
                        true,
                        "after_message_stream_start",
                        request_bytes,
                    );
                    Err(error.with_diagnostic(diagnostic))
                }
            };
            let finished = item.is_err() || semantic_frame_terminates_adapted_body(&item);
            Some((
                item,
                (
                    body,
                    state,
                    session_id,
                    connection,
                    full_request,
                    configured_transport,
                    request_bytes,
                    output,
                    continuation_guard,
                    finished,
                ),
            ))
        },
    ))
}

async fn execute_codex_websocket_send(
    websocket: &dyn OpenAiCodexWebSocketTransport,
    state: &Arc<Mutex<CodexWebSocketState>>,
    mut request: HttpRequest,
    cached: bool,
    cancellation: CancellationToken,
) -> Result<HttpResponse, pi_ai::TransportError> {
    let configured_transport = request.transport.unwrap_or_default();
    let request_bytes = request.body.len();
    let account_id = codex_account_id(&request)?;
    let session_id = codex_session_id(&request);
    let websocket_request_id = pi_ai::clamp_openai_prompt_cache_key(session_id.as_deref())
        .unwrap_or_else(next_codex_websocket_request_id);
    let full_request = codex_request_json(&request.body)?;
    let full_frame = codex_response_create_frame(full_request.clone())?;
    let use_cached_context = cached && session_id.is_some();
    prepare_codex_websocket_headers(
        &mut request.headers,
        &request.auth_headers,
        &websocket_request_id,
    );
    let mut retry_connection_limit = true;
    let mut retry_missing_continuation = true;
    'attempt: loop {
        let exchange = OpenAiCodexWebSocketRequest {
            url: codex_websocket_url(&request.url),
            headers: request.headers.clone(),
            body: full_frame.clone(),
            session_id: session_id.clone(),
            account_id: account_id.clone(),
            connect_timeout: request.websocket_connect_timeout,
            idle_timeout: request.timeout,
            use_cached_context,
        };
        match websocket.execute(exchange, cancellation.clone()).await {
            Ok(response) => {
                let connection = response.connection;
                let mut body = response.body;
                let mut prefetched = Vec::new();
                loop {
                    let next =
                        next_codex_websocket_frame_send(&mut body, &cancellation, request.timeout)
                            .await;
                    let frame = match next {
                        Ok(Some(frame)) => frame,
                        Ok(None) if prefetched.is_empty() => {
                            connection.replace_continuation(None);
                            return Err(pi_ai::TransportError::new(
                                "openai_codex_websocket_closed_before_start",
                                "Codex WebSocket closed before its first response event",
                            ));
                        }
                        Ok(None) => {
                            body = Box::pin(stream::empty());
                            break;
                        }
                        Err(error) if prefetched.is_empty() => {
                            connection.replace_continuation(None);
                            return Err(error);
                        }
                        Err(error) => {
                            body = Box::pin(stream::once(async move { Err(error) }));
                            break;
                        }
                    };
                    let event_code = codex_event_error_code(&frame);
                    prefetched.push(frame);
                    if event_code.as_deref() == Some(CODEX_PREVIOUS_RESPONSE_NOT_FOUND)
                        && retry_missing_continuation
                    {
                        retry_missing_continuation = false;
                        connection.replace_continuation(None);
                        continue 'attempt;
                    }
                    if event_code.as_deref() == Some(CODEX_WEBSOCKET_CONNECTION_LIMIT)
                        && prefetched.len() == 1
                    {
                        connection.replace_continuation(None);
                        if retry_connection_limit {
                            retry_connection_limit = false;
                            continue 'attempt;
                        }
                        return Err(pi_ai::TransportError::new(
                            CODEX_WEBSOCKET_CONNECTION_LIMIT,
                            "Codex WebSocket connection limit was reached before output started",
                        ));
                    }
                    if !codex_is_rate_limits_event(prefetched.last().expect("frame pushed")) {
                        break;
                    }
                }
                let body = Box::pin(stream::iter(prefetched.into_iter().map(Ok)).chain(body));
                return Ok(HttpResponse {
                    status: 200,
                    headers: HeaderMap::new(),
                    diagnostics: Vec::new(),
                    notify_observers: false,
                    body: adapt_send_websocket_body(
                        body,
                        Arc::clone(state),
                        session_id.clone(),
                        use_cached_context.then_some(connection),
                        full_request,
                        configured_transport,
                        request_bytes,
                    ),
                });
            }
            Err(error)
                if error.code == CODEX_PREVIOUS_RESPONSE_NOT_FOUND
                    && retry_missing_continuation =>
            {
                retry_missing_continuation = false;
            }
            Err(error)
                if error.code == CODEX_WEBSOCKET_CONNECTION_LIMIT && retry_connection_limit =>
            {
                retry_connection_limit = false;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn execute_codex_websocket_local(
    websocket: &dyn LocalOpenAiCodexWebSocketTransport,
    state: &Rc<RefCell<CodexWebSocketState>>,
    mut request: HttpRequest,
    cached: bool,
    cancellation: CancellationToken,
) -> Result<LocalHttpResponse, pi_ai::TransportError> {
    let configured_transport = request.transport.unwrap_or_default();
    let request_bytes = request.body.len();
    let account_id = codex_account_id(&request)?;
    let session_id = codex_session_id(&request);
    let websocket_request_id = pi_ai::clamp_openai_prompt_cache_key(session_id.as_deref())
        .unwrap_or_else(next_codex_websocket_request_id);
    let full_request = codex_request_json(&request.body)?;
    let full_frame = codex_response_create_frame(full_request.clone())?;
    let use_cached_context = cached && session_id.is_some();
    prepare_codex_websocket_headers(
        &mut request.headers,
        &request.auth_headers,
        &websocket_request_id,
    );
    let mut retry_connection_limit = true;
    let mut retry_missing_continuation = true;
    'attempt: loop {
        let exchange = OpenAiCodexWebSocketRequest {
            url: codex_websocket_url(&request.url),
            headers: request.headers.clone(),
            body: full_frame.clone(),
            session_id: session_id.clone(),
            account_id: account_id.clone(),
            connect_timeout: request.websocket_connect_timeout,
            idle_timeout: request.timeout,
            use_cached_context,
        };
        match websocket.execute(exchange, cancellation.clone()).await {
            Ok(response) => {
                let connection = response.connection;
                let mut body = response.body;
                let mut prefetched = Vec::new();
                loop {
                    let next =
                        next_codex_websocket_frame_local(&mut body, &cancellation, request.timeout)
                            .await;
                    let frame = match next {
                        Ok(Some(frame)) => frame,
                        Ok(None) if prefetched.is_empty() => {
                            connection.replace_continuation(None);
                            return Err(pi_ai::TransportError::new(
                                "openai_codex_websocket_closed_before_start",
                                "Codex WebSocket closed before its first response event",
                            ));
                        }
                        Ok(None) => {
                            body = Box::pin(stream::empty());
                            break;
                        }
                        Err(error) if prefetched.is_empty() => {
                            connection.replace_continuation(None);
                            return Err(error);
                        }
                        Err(error) => {
                            body = Box::pin(stream::once(async move { Err(error) }));
                            break;
                        }
                    };
                    let event_code = codex_event_error_code(&frame);
                    prefetched.push(frame);
                    if event_code.as_deref() == Some(CODEX_PREVIOUS_RESPONSE_NOT_FOUND)
                        && retry_missing_continuation
                    {
                        retry_missing_continuation = false;
                        connection.replace_continuation(None);
                        continue 'attempt;
                    }
                    if event_code.as_deref() == Some(CODEX_WEBSOCKET_CONNECTION_LIMIT)
                        && prefetched.len() == 1
                    {
                        connection.replace_continuation(None);
                        if retry_connection_limit {
                            retry_connection_limit = false;
                            continue 'attempt;
                        }
                        return Err(pi_ai::TransportError::new(
                            CODEX_WEBSOCKET_CONNECTION_LIMIT,
                            "Codex WebSocket connection limit was reached before output started",
                        ));
                    }
                    if !codex_is_rate_limits_event(prefetched.last().expect("frame pushed")) {
                        break;
                    }
                }
                let body = Box::pin(stream::iter(prefetched.into_iter().map(Ok)).chain(body));
                return Ok(LocalHttpResponse {
                    status: 200,
                    headers: HeaderMap::new(),
                    diagnostics: Vec::new(),
                    notify_observers: false,
                    body: adapt_local_websocket_body(
                        body,
                        Rc::clone(state),
                        session_id.clone(),
                        use_cached_context.then_some(connection),
                        full_request,
                        configured_transport,
                        request_bytes,
                    ),
                });
            }
            Err(error)
                if error.code == CODEX_PREVIOUS_RESPONSE_NOT_FOUND
                    && retry_missing_continuation =>
            {
                retry_missing_continuation = false;
            }
            Err(error)
                if error.code == CODEX_WEBSOCKET_CONNECTION_LIMIT && retry_connection_limit =>
            {
                retry_connection_limit = false;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn next_codex_websocket_frame_send(
    body: &mut HttpBody,
    cancellation: &CancellationToken,
    idle_timeout: Option<std::time::Duration>,
) -> Result<Option<Vec<u8>>, pi_ai::TransportError> {
    if cancellation.is_cancelled() {
        return Err(pi_ai::TransportError::new(
            "cancelled",
            "Codex WebSocket request was cancelled",
        ));
    }
    let next = body.next().fuse();
    let cancelled = cancellation.cancelled().fuse();
    let timeout = async move {
        match idle_timeout {
            Some(timeout) if !timeout.is_zero() => futures_timer::Delay::new(timeout).await,
            _ => future::pending().await,
        }
    }
    .fuse();
    futures_util::pin_mut!(next, cancelled, timeout);
    futures_util::select_biased! {
        _ = cancelled => Err(pi_ai::TransportError::new(
            "cancelled",
            "Codex WebSocket request was cancelled",
        )),
        _ = timeout => Err(pi_ai::TransportError::new(
            "openai_codex_websocket_idle_before_start",
            "Codex WebSocket was idle before its first response event",
        )),
        item = next => item.transpose(),
    }
}

async fn next_codex_websocket_frame_local(
    body: &mut LocalHttpBody,
    cancellation: &CancellationToken,
    idle_timeout: Option<std::time::Duration>,
) -> Result<Option<Vec<u8>>, pi_ai::TransportError> {
    if cancellation.is_cancelled() {
        return Err(pi_ai::TransportError::new(
            "cancelled",
            "Codex WebSocket request was cancelled",
        ));
    }
    let next = body.next().fuse();
    let cancelled = cancellation.cancelled().fuse();
    let timeout = async move {
        match idle_timeout {
            Some(timeout) if !timeout.is_zero() => futures_timer::Delay::new(timeout).await,
            _ => future::pending().await,
        }
    }
    .fuse();
    futures_util::pin_mut!(next, cancelled, timeout);
    futures_util::select_biased! {
        _ = cancelled => Err(pi_ai::TransportError::new(
            "cancelled",
            "Codex WebSocket request was cancelled",
        )),
        _ = timeout => Err(pi_ai::TransportError::new(
            "openai_codex_websocket_idle_before_start",
            "Codex WebSocket was idle before its first response event",
        )),
        item = next => item.transpose(),
    }
}

fn codex_event_error_code(frame: &[u8]) -> Option<String> {
    let event: serde_json::Value = serde_json::from_slice(frame).ok()?;
    match event.get("type").and_then(serde_json::Value::as_str) {
        Some("error") => event
            .get("code")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                event
                    .get("error")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|error| error.get("code"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_owned),
        Some("response.failed") => event
            .get("response")
            .and_then(serde_json::Value::as_object)
            .and_then(|response| response.get("error"))
            .and_then(serde_json::Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn codex_is_semantic_error_event(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|event| {
            event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(|event_type| matches!(event_type, "error" | "response.failed"))
        })
        .unwrap_or(false)
}

fn codex_is_terminal_event(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|event| {
            event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(|event_type| {
                    matches!(
                        event_type,
                        "response.done" | "response.completed" | "response.incomplete"
                    )
                })
        })
        .unwrap_or(false)
}

fn codex_is_rate_limits_event(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|event| {
            event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("codex.rate_limits")
}

impl fmt::Debug for LocalOpenAiCodexResponsesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalOpenAiCodexResponsesTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalOpenAiCodexResponsesTransport {
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, pi_ai::TransportError>> {
        Box::pin(self.execute_selected(request, cancellation))
    }
}

impl LocalOpenAiCodexResponsesTransport {
    async fn execute_selected(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> Result<LocalHttpResponse, pi_ai::TransportError> {
        let selection = request.transport.unwrap_or_default();
        // Keep the pre-stream WebSocket-to-SSE decision for the remainder of
        // this logical request. `attempt` is assigned by the API retry loop and
        // works for one-shot requests without session affinity.
        if selection == StreamTransport::Sse || request.attempt > 0 {
            return self.execute_sse(request, cancellation).await;
        }

        let session_id = codex_session_id(&request);
        let fallback_active = session_id.as_ref().is_some_and(|session_id| {
            self.websocket_state
                .borrow()
                .sse_fallback_sessions
                .contains(session_id)
        });
        if fallback_active {
            return self.execute_sse(request, cancellation).await;
        }
        let Some(websocket) = self.websocket.as_ref() else {
            record_local_codex_sse_fallback(&self.websocket_state, session_id.as_deref());
            let diagnostic = codex_fallback_diagnostic(
                selection,
                &request,
                &pi_ai::TransportError::new(
                    "openai_codex_websocket_unavailable",
                    "WebSocket transport is not available in this runtime",
                ),
            );
            return match self.execute_sse(request, cancellation).await {
                Ok(mut response) => {
                    response.diagnostics.push(diagnostic);
                    Ok(response)
                }
                Err(error) => Err(error.with_diagnostic(diagnostic)),
            };
        };
        let cached = matches!(
            selection,
            StreamTransport::Auto | StreamTransport::WebsocketCached
        );
        match execute_codex_websocket_local(
            websocket.as_ref(),
            &self.websocket_state,
            request.clone(),
            cached,
            cancellation.clone(),
        )
        .await
        {
            Ok(response) => Ok(response),
            Err(error) if cancellation.is_cancelled() => Err(error),
            Err(error) => {
                let diagnostic = codex_fallback_diagnostic(selection, &request, &error);
                record_local_codex_sse_fallback(&self.websocket_state, session_id.as_deref());
                match self.execute_sse(request, cancellation).await {
                    Ok(mut response) => {
                        response.diagnostics.push(diagnostic);
                        Ok(response)
                    }
                    Err(error) => Err(error.with_diagnostic(diagnostic)),
                }
            }
        }
    }

    async fn execute_sse(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> Result<LocalHttpResponse, pi_ai::TransportError> {
        prepare_codex_sse_request(&mut request);
        self.inner.execute(request, cancellation).await
    }
}

fn codex_fallback_diagnostic(
    transport: StreamTransport,
    request: &HttpRequest,
    error: &pi_ai::TransportError,
) -> AssistantMessageDiagnostic {
    codex_transport_diagnostic(
        transport,
        error,
        Some("sse"),
        false,
        "before_message_stream_start",
        request.body.len(),
    )
}

fn codex_transport_diagnostic(
    transport: StreamTransport,
    error: &pi_ai::TransportError,
    fallback_transport: Option<&str>,
    events_emitted: bool,
    phase: &str,
    request_bytes: usize,
) -> AssistantMessageDiagnostic {
    let configured_transport = match transport {
        StreamTransport::Auto => "auto",
        StreamTransport::Sse => "sse",
        StreamTransport::Websocket => "websocket",
        StreamTransport::WebsocketCached => "websocket-cached",
    };
    let mut details = BTreeMap::new();
    details.insert(
        "configuredTransport".into(),
        serde_json::Value::String(configured_transport.into()),
    );
    if let Some(fallback_transport) = fallback_transport {
        details.insert(
            "fallbackTransport".into(),
            serde_json::Value::String(fallback_transport.into()),
        );
    }
    details.insert(
        "eventsEmitted".into(),
        serde_json::Value::Bool(events_emitted),
    );
    details.insert("phase".into(), serde_json::Value::String(phase.into()));
    details.insert(
        "requestBytes".into(),
        serde_json::Value::from(u64::try_from(request_bytes).unwrap_or(u64::MAX)),
    );
    AssistantMessageDiagnostic {
        schema_version: ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
        kind: "provider_transport_failure".into(),
        timestamp: now_timestamp(),
        error: Some(DiagnosticErrorInfo {
            name: Some("TransportError".into()),
            message: error.message.clone(),
            stack: None,
            code: Some(DiagnosticErrorCode::String(error.code.clone())),
        }),
        details,
    }
}

fn responses_url(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        url.set_path(&format!("{path}/responses"));
    }
    url
}

fn codex_responses_url(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    if path.ends_with("/codex/responses") {
        return url;
    }
    if path.ends_with("/codex") {
        url.set_path(&format!("{path}/responses"));
    } else {
        url.set_path(&format!("{path}/codex/responses"));
    }
    url
}

fn ensure_responses_headers(headers: &mut HeaderMap, codex: bool) {
    if !headers.contains_key(header::USER_AGENT) {
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(PI_AI_RUST_USER_AGENT),
        );
    }
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if codex {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if codex {
        headers.insert(
            "openai-beta",
            HeaderValue::from_static("responses=experimental"),
        );
    }
}

fn reassert_codex_headers(headers: &mut HeaderMap, auth_headers: &HeaderMap) {
    // Reinsert the immutable credential contribution, not the already
    // overlaid logical value. This is what preserves OAuth identity when a
    // model, caller, transform, or attempt middleware tries to replace it.
    for name in [
        header::AUTHORIZATION,
        http::HeaderName::from_static("chatgpt-account-id"),
        http::HeaderName::from_static("originator"),
        header::USER_AGENT,
    ] {
        if let Some(value) = auth_headers.get(&name).cloned() {
            headers.insert(name, value);
        }
    }
}

fn reassert_codex_session_headers(headers: &mut HeaderMap, session_id: Option<&str>) {
    let Some(session_id) = pi_ai::clamp_openai_prompt_cache_key(session_id)
        .and_then(|value| HeaderValue::from_str(&value).ok())
    else {
        return;
    };
    headers.insert("session-id", session_id.clone());
    headers.insert("x-client-request-id", session_id);
}

fn compress_codex_request(request: &mut HttpRequest) {
    // Pinned Pi asks Node zlib for a level-three Zstandard frame and falls
    // back to the logical JSON body only if compression is unavailable.
    if let Ok(compressed) = zstd::stream::encode_all(request.body.as_slice(), 3) {
        request.body = compressed;
        request
            .headers
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    }
}
