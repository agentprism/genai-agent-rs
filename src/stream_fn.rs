//! Injectable provider streaming boundary and its process-global fallback.
//!
//! [`StreamFn`] is deliberately a never-throw interface: setup failures, stream failures, and
//! cancellation are represented by terminal assistant events. [`GenaiStreamFn`] provides the
//! production adapter, while [`set_default_stream_fn`] configures the fallback shared by all agents
//! in the process.

use crate::{
    AssistantAccumulator, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream,
    OnPayloadHook, OnResponseHook, PriceCatalog, StopReason, StreamResponseInfo, Transport,
    compute_cost,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatStream, ChatStreamEvent, Tool};
use genai::{
    Client, ClientBuilder, ExecOptions, ModelIden, ModelSpec, PayloadInterceptor, ResponseObserver,
};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Provider-facing context after `AgentMessage` transformation and conversion.
#[derive(Debug, Clone, Default)]
pub struct LlmContext {
    /// System instruction sent ahead of the conversation; an empty string is omitted.
    pub system_prompt: String,
    /// Provider-native chat messages after transcript conversion.
    pub messages: Vec<ChatMessage>,
    /// Provider-visible tool definitions; an empty list is omitted.
    pub tools: Vec<Tool>,
}

impl LlmContext {
    /// Consume the context and construct a `genai` chat request.
    ///
    /// Empty system prompts and tool lists remain absent rather than becoming explicitly empty
    /// request fields.
    pub fn into_chat_request(self) -> ChatRequest {
        let mut request = ChatRequest::from_messages(self.messages);
        if !self.system_prompt.is_empty() {
            request.system = Some(self.system_prompt);
        }
        if !self.tools.is_empty() {
            request.tools = Some(self.tools);
        }
        request
    }
}

/// One invocation captured at the sole provider boundary.
#[derive(Clone)]
#[non_exhaustive]
pub struct StreamRequest {
    /// Model name, identity, or fully targeted provider selection.
    pub model: ModelSpec,
    /// Converted prompt, transcript, and tool definitions for this invocation.
    pub context: LlmContext,
    /// Per-invocation provider options.
    pub options: ChatOptions,
    /// Preferred provider transport advisory for this invocation.
    ///
    /// A custom [`StreamFn`] may honor this hint. The production [`GenaiStreamFn`] ignores it
    /// because genai speaks SSE only; the TypeScript contract states that providers which do not
    /// support the requested transport ignore it, so ignoring it is compliant.
    pub transport: Transport,
    /// Optional pre-send payload hook for this invocation (pi's `onPayload`).
    ///
    /// The agent loop forwards the configured hook here. [`GenaiStreamFn`] honors it as a
    /// request-scoped **replacement** of its construction-time hook (via the genai fork's
    /// request-level [`ExecOptions`]): when set, the construction-time hook does not fire for the
    /// request, and the hook fires exactly once per physical attempt, including retries. When
    /// absent, the construction-time hook (if any) applies unchanged. Custom [`StreamFn`]
    /// implementations and the `proxy`-feature `ProxyStreamFn` honor the forwarded hook directly.
    pub on_payload: Option<OnPayloadHook>,
    /// Optional response observation hook for this invocation (pi's `onResponse`).
    ///
    /// The agent loop forwards the configured hook here. [`GenaiStreamFn`] honors it as a
    /// request-scoped **replacement** of its construction-time hook (via the genai fork's
    /// request-level [`ExecOptions`]): when set, the construction-time hook does not fire for the
    /// request, and the hook fires exactly once per physical attempt — on the response head,
    /// including 4xx/5xx responses and retry attempts. When absent, the construction-time hook
    /// (if any) applies unchanged. Custom [`StreamFn`] implementations and the `proxy`-feature
    /// `ProxyStreamFn` honor the forwarded hook directly.
    pub on_response: Option<OnResponseHook>,
    /// Optional session identifier for this invocation (pi's `StreamOptions.sessionId`).
    ///
    /// The identifier reaches this per-execution context for correlation (logging, metrics,
    /// session-aware routing in custom stream functions). The production [`GenaiStreamFn`]
    /// deliberately does not serialize it: provider serialization is genai's concern and the
    /// explicit cache-affinity path there is `ChatOptions::prompt_cache_key`, which is
    /// **independent** of this field — setting or clearing `session_id` never writes
    /// `options.prompt_cache_key`, and vice versa, so the value never enters provider JSON.
    pub session_id: Option<String>,
    /// Optional per-request maximum number of provider-handshake retries (pi's
    /// `StreamOptions.maxRetries`).
    ///
    /// [`GenaiStreamFn`] honors this as an override of its construction-time
    /// [`RetryPolicy::max_retries`] (`Some(0)` disables retries for the request). Custom
    /// [`StreamFn`] implementations may honor or ignore it.
    pub max_retries: Option<u32>,
    /// Optional per-request cap, in milliseconds, on a *server-requested* retry delay (pi's
    /// `StreamOptions.maxRetryDelayMs`).
    ///
    /// [`GenaiStreamFn`] honors this as an override of its construction-time
    /// [`RetryPolicy::max_retry_delay_ms`]. Custom [`StreamFn`] implementations may honor or
    /// ignore it.
    pub max_retry_delay_ms: Option<u64>,
    /// Cooperative cancellation token for setup and streaming.
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for StreamRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRequest")
            .field("model", &self.model)
            .field("context", &self.context)
            .field("options", &self.options)
            .field("transport", &self.transport)
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
            .field("session_id", &self.session_id)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("cancel", &self.cancel)
            .finish_non_exhaustive()
    }
}

impl StreamRequest {
    /// Construct a request with default chat options, the default transport, no exec hooks, and a
    /// fresh cancellation token.
    pub fn new(model: impl Into<ModelSpec>, context: LlmContext) -> Self {
        Self {
            model: model.into(),
            context,
            options: ChatOptions::default(),
            transport: Transport::default(),
            on_payload: None,
            on_response: None,
            session_id: None,
            max_retries: None,
            max_retry_delay_ms: None,
            cancel: CancellationToken::new(),
        }
    }

    /// Replace the per-invocation chat options.
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.options = options;
        self
    }

    /// Replace the preferred provider transport advisory.
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Install the per-invocation pre-send payload hook (see [`StreamRequest::on_payload`]).
    pub fn with_on_payload(mut self, on_payload: OnPayloadHook) -> Self {
        self.on_payload = Some(on_payload);
        self
    }

    /// Install the per-invocation response observation hook (see [`StreamRequest::on_response`]).
    pub fn with_on_response(mut self, on_response: OnResponseHook) -> Self {
        self.on_response = Some(on_response);
        self
    }

    /// Set the per-invocation session identifier (see [`StreamRequest::session_id`]).
    ///
    /// This never writes `options.prompt_cache_key`; the two are independent.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the per-invocation maximum number of provider-handshake retries (see
    /// [`StreamRequest::max_retries`]).
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Set the per-invocation server-requested retry-delay cap in milliseconds (see
    /// [`StreamRequest::max_retry_delay_ms`]).
    pub fn with_max_retry_delay_ms(mut self, max_retry_delay_ms: u64) -> Self {
        self.max_retry_delay_ms = Some(max_retry_delay_ms);
        self
    }

    /// Use the supplied cooperative cancellation token.
    pub fn with_cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }
}

/// Injectable provider boundary for the agent loop.
///
/// This interface has an in-band, never-throw contract: recoverable setup and runtime failures must
/// become a stream ending in [`crate::AssistantMessageEvent::Error`], and observed cancellation
/// must end in [`crate::AssistantMessageEvent::Error`] with [`StopReason::Aborted`]. Implementations
/// must not use panics as an error path and must not close a returned stream without a terminal
/// event. The absence of a `Result` in [`Self::stream`] makes that boundary explicit.
#[async_trait]
pub trait StreamFn: Send + Sync {
    /// Start one provider invocation and always return its assistant event stream.
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream;
}

#[async_trait]
impl<F, Fut> StreamFn for F
where
    F: Fn(StreamRequest) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = AssistantMessageEventStream> + Send,
{
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream {
        (self)(request).await
    }
}

/// pi-ai's default server-requested retry-delay cap (`provider-retry.ts:1`,
/// `DEFAULT_MAX_RETRY_DELAY_MS = 60_000`). Also used when [`RetryPolicy::max_retry_delay_ms`] is
/// its default. A cap of `0` disables the limit, mirroring pi's `maxDelayMs > 0` guard.
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Exponential-backoff constants copied verbatim from pi-ai `provider-retry.ts:65-66`:
/// `Math.min(0.5 * 2 ** retryIndex, 8) * 1000 * (1 - Math.random() * 0.25)`.
const RETRY_BASE_DELAY_SECS: f64 = 0.5;
const RETRY_MAX_BACKOFF_SECS: f64 = 8.0;
const RETRY_JITTER_FRACTION: f64 = 0.25;

/// Retry policy for [`GenaiStreamFn`]'s initial request/handshake, mirroring pi-ai's provider retry
/// policy (`packages/ai/src/utils/provider-retry.ts`).
///
/// The policy wraps **only** the request creation up to the first stream event — the handshake.
/// Once any content event has been observed, no later mid-stream failure is retried, exactly like
/// pi wrapping only `request()` (up to response receipt) with `retryProviderRequest`.
///
/// A failure is retried when it is a streaming HTTP handshake error (an in-band terminal error that
/// downcasts to [`genai::Error::HttpError`]) whose status is `408`, `409`, `429`, or `>= 500`, or
/// whose `x-should-retry` header is `true`; an `x-should-retry: false` header is a hard no-retry
/// override even on an otherwise-retryable status. The retry delay follows the precedence
/// `retry-after-ms` (float milliseconds) > `retry-after` (float seconds or an HTTP-date) >
/// exponential backoff with jitter. A *server-requested* delay above [`Self::max_retry_delay_ms`]
/// fails immediately with pi's exact message; the computed exponential backoff is never capped.
///
/// The delay sleep is cancellation-aware: a cancel during the sleep aborts in-band with
/// [`crate::StopReason::Aborted`] instead of sleeping on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of retries of the initial handshake. `0` (the default) disables retries, so
    /// behavior is byte-identical to the pre-retry path (mirrors pi's `maxRetries ?? 0`).
    pub max_retries: u32,
    /// Cap, in milliseconds, on a *server-requested* retry delay (`retry-after`/`retry-after-ms`).
    /// A server-requested delay above this fails immediately rather than sleeping. `0` disables the
    /// cap. Defaults to pi's `DEFAULT_MAX_RETRY_DELAY_MS` (60000). The computed exponential backoff
    /// is never subject to this cap.
    pub max_retry_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            max_retry_delay_ms: DEFAULT_MAX_RETRY_DELAY_MS,
        }
    }
}

/// `genai::Client` adapter for the provider-neutral assistant event protocol.
///
/// Capture options are forced on every invocation because `StreamEnd` is the authoritative source
/// for final content, parsed tool arguments, usage, stop reason, and response id.
///
/// `on_payload`/`on_response` exec hooks can be installed at construction time (see
/// [`GenaiStreamFn::with_exec_hooks`]) **and** per request ([`StreamRequest::on_payload`] /
/// [`StreamRequest::on_response`]): a request hook replaces the construction-time hook of its
/// channel for that execution only (never composing, so exactly one hook fires per channel per
/// physical attempt), while an absent request hook inherits the construction-time default.
#[derive(Clone)]
pub struct GenaiStreamFn {
    /// `genai` client used for provider execution.
    pub client: Client,
    /// Options inherited by invocations unless their corresponding request field overrides them.
    pub base_options: ChatOptions,
    /// Optional model price catalog used to attach monetary cost at stream finalization.
    ///
    /// When set, the authoritative terminal message's usage gets [`crate::AgentUsage::cost`]
    /// populated via [`compute_cost`] for the models the catalog prices. When absent, cost stays
    /// `None`.
    pub price_catalog: Option<Arc<dyn PriceCatalog>>,
    /// Retry policy for the initial request/handshake (see [`RetryPolicy`]).
    ///
    /// Defaults to [`RetryPolicy::default`] (`max_retries = 0`), so retries are opt-in via
    /// [`GenaiStreamFn::with_retry`] and the default behavior is byte-identical to the pre-retry
    /// path. A request's [`StreamRequest::max_retries`]/[`StreamRequest::max_retry_delay_ms`]
    /// `Some` values override the corresponding policy field for that invocation, mirroring how
    /// pi forwards `maxRetries`/`maxRetryDelayMs` as per-request stream options.
    pub retry: RetryPolicy,
}

impl std::fmt::Debug for GenaiStreamFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiStreamFn")
            .field("client", &self.client)
            .field("base_options", &self.base_options)
            .field("price_catalog", &self.price_catalog.is_some())
            .field("retry", &self.retry)
            .finish()
    }
}

impl GenaiStreamFn {
    /// Construct an adapter with default base chat options, no price catalog, and retries disabled.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_options: ChatOptions::default(),
            price_catalog: None,
            retry: RetryPolicy::default(),
        }
    }

    /// Construct an adapter whose `genai` client is built from `builder` with construction-time
    /// `on_payload`/`on_response` exec hooks installed.
    ///
    /// The genai fork's exec hooks ([`PayloadInterceptor`] / [`ResponseObserver`]) are installed
    /// on the built [`Client`] as the construction-time defaults for their channels — mirroring
    /// how pi wires `onPayload`/`onResponse` once at its production `Agent` construction site. A
    /// per-request [`StreamRequest::on_payload`]/[`StreamRequest::on_response`] hook **replaces**
    /// the corresponding construction-time hook for that execution (the two never compose);
    /// requests that carry no hook inherit these defaults unchanged.
    ///
    /// `on_payload` fires with the serialized provider payload before the HTTP request is built
    /// (`Some` replaces the wire payload). `on_response` fires with the response status and
    /// headers as soon as the HTTP response arrives — before its body/stream is consumed and also
    /// on 4xx/5xx. On genai's streaming path the HTTP send is lazy, so both fire during the first
    /// stream poll rather than at [`StreamFn::stream`] return time. Under retries, each re-issued
    /// attempt resolves and fires the hooks again, exactly once per physical attempt.
    ///
    /// Applications that need additional client configuration (auth resolvers, custom reqwest
    /// clients, ...) apply it to the supplied `builder`; alternatively they can install
    /// [`payload_interceptor_from_hook`] / [`response_observer_from_hook`] on their own
    /// `ClientBuilder` and use [`GenaiStreamFn::new`].
    pub fn with_exec_hooks(
        mut builder: ClientBuilder,
        on_payload: Option<OnPayloadHook>,
        on_response: Option<OnResponseHook>,
    ) -> Self {
        if let Some(on_payload) = on_payload {
            builder = builder.with_payload_interceptor(payload_interceptor_from_hook(on_payload));
        }
        if let Some(on_response) = on_response {
            builder = builder.with_response_observer(response_observer_from_hook(on_response));
        }
        Self::new(builder.build())
    }

    /// Replace the options inherited by future invocations.
    ///
    /// Per-request `Some` fields override these values; required final-content, tool-call, usage,
    /// and reasoning capture is forced on after option merging.
    pub fn with_base_options(mut self, options: ChatOptions) -> Self {
        self.base_options = options;
        self
    }

    /// Install a model price catalog.
    ///
    /// With a catalog set, the final assistant message produced by an invocation has its usage
    /// cost computed and attached at stream finalization (see [`attach_cost`]). Models the catalog
    /// does not price, and every invocation while no catalog is set, leave the cost `None`.
    pub fn with_price_catalog(mut self, price_catalog: Arc<dyn PriceCatalog>) -> Self {
        self.price_catalog = Some(price_catalog);
        self
    }

    /// Install a [`RetryPolicy`] for the initial request/handshake.
    ///
    /// With `max_retries > 0`, a failed streaming handshake whose in-band terminal error downcasts
    /// to a retryable [`genai::Error::HttpError`] is retried after a pi-mirroring delay: the whole
    /// request is re-issued up to `max_retries` times. Only the handshake (up to the first stream
    /// event) is wrapped; once content has been emitted no later failure is retried. The retry-delay
    /// sleep is cancellation-aware. The default policy (`max_retries = 0`) leaves behavior
    /// byte-identical to the pre-retry path. See [`RetryPolicy`] for the exact classification, delay
    /// precedence, and cap semantics reproduced from pi-ai's `provider-retry.ts`.
    ///
    /// Per-request [`StreamRequest::max_retries`]/[`StreamRequest::max_retry_delay_ms`] `Some`
    /// values override the corresponding policy field for that invocation.
    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }
}

#[async_trait]
impl StreamFn for GenaiStreamFn {
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream {
        let StreamRequest {
            model,
            context,
            options,
            // genai speaks SSE only, so the transport advisory is intentionally ignored here. The
            // TypeScript contract makes ignoring an unsupported transport compliant.
            transport: _,
            on_payload,
            on_response,
            // Provider session/cache-affinity serialization is genai's concern
            // (`ChatOptions::prompt_cache_key` is the explicit path); the session id reaches this
            // per-execution context for correlation only and is deliberately never serialized into
            // provider payloads (see the `StreamRequest::session_id` field docs).
            session_id: _,
            max_retries,
            max_retry_delay_ms,
            cancel,
        } = request;
        let error_model = model_iden_for_error(&model);
        let chat_request = context.into_chat_request();
        let options = force_capture_options(overlay_chat_options(&self.base_options, options));
        let price_catalog = self.price_catalog.clone();

        // Per-request `Some` values overlay the construction-time retry policy, mirroring pi's
        // per-request `maxRetries`/`maxRetryDelayMs` stream options.
        let retry = RetryPolicy {
            max_retries: max_retries.unwrap_or(self.retry.max_retries),
            max_retry_delay_ms: max_retry_delay_ms.unwrap_or(self.retry.max_retry_delay_ms),
        };

        // Per-request exec hooks overlay the client's construction-time hooks as *replacements*
        // (one resolved hook per channel per physical attempt; the two never compose). Absent
        // request hooks leave the channels in `Inherit` state, so construction defaults apply
        // unchanged. Built once per invocation and reused across retry attempts.
        let mut exec_options = ExecOptions::new();
        if let Some(on_payload) = on_payload {
            exec_options =
                exec_options.with_payload_interceptor(payload_interceptor_from_hook(on_payload));
        }
        if let Some(on_response) = on_response {
            exec_options =
                exec_options.with_response_observer(response_observer_from_hook(on_response));
        }

        // Retries disabled (the default): the original single-shot path, byte-identical to the
        // pre-retry behavior. The HTTP send stays lazy (performed on the first stream poll).
        if retry.max_retries == 0 {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return AssistantMessageEventStream::from_error(AssistantMessage::error(
                        error_model,
                        StopReason::Aborted,
                        "Request aborted by user",
                    ));
                }
                response = self.client.exec_chat_stream_with_exec_options(
                    model,
                    chat_request,
                    Some(&options),
                    Some(&exec_options),
                ) => response,
            };

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    return AssistantMessageEventStream::from_error(AssistantMessage::error(
                        error_model,
                        StopReason::Error,
                        error.to_string(),
                    ));
                }
            };

            let accumulator = AssistantAccumulator::new(response.model_iden);
            return finalize_event_stream(
                response.stream,
                accumulator,
                cancel,
                VecDeque::new(),
                false,
                price_catalog,
            );
        }

        // Retries enabled: peek the first stream event and re-issue the whole request while the
        // handshake keeps failing with a retryable HTTP error. Peeking eagerly performs the HTTP
        // send here (rather than on the first stream poll), which is required to classify the
        // handshake before any content is emitted.
        let RetryPolicy {
            max_retries,
            max_retry_delay_ms,
        } = retry;
        let mut attempt: u32 = 0;
        loop {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return AssistantMessageEventStream::from_error(AssistantMessage::error(
                        error_model,
                        StopReason::Aborted,
                        "Request aborted by user",
                    ));
                }
                response = self.client.exec_chat_stream_with_exec_options(
                    model.clone(),
                    chat_request.clone(),
                    Some(&options),
                    Some(&exec_options),
                ) => response,
            };

            let mut response = match response {
                Ok(response) => response,
                Err(error) => {
                    // A setup failure (auth/model mapping) is not an in-band HTTP handshake error
                    // and is never retryable; surface it as the terminal error, exactly as the
                    // no-retry path does.
                    return AssistantMessageEventStream::from_error(AssistantMessage::error(
                        error_model,
                        StopReason::Error,
                        error.to_string(),
                    ));
                }
            };

            // Peek past any leading synthetic `Start` events to the actual handshake result,
            // cancel-aware. genai's SSE `EventSourceStream` emits `Event::Open` ->
            // `ChatStreamEvent::Start` *before* the HTTP send resolves, so the handshake result
            // (content, an in-band error, or an immediate close) is the first non-`Start` event. A
            // discarded `Start` is losslessly re-synthesized by the accumulator (`fold`/`fail` emit
            // `Start` when needed), so a pass-through — and a retry that re-issues a fresh request —
            // both stay correct. `cancelled_first` distinguishes a cancel during the handshake from
            // a stream that produced no non-`Start` event at all.
            let mut cancelled_first = false;
            let mut peeked: Option<Result<ChatStreamEvent, genai::Error>> = None;
            loop {
                let mut item: Option<Result<ChatStreamEvent, genai::Error>> = None;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => cancelled_first = true,
                    next = response.stream.next() => item = next,
                }
                if cancelled_first {
                    break;
                }
                if let Some(Ok(ChatStreamEvent::Start)) = item {
                    continue;
                }
                peeked = item;
                break;
            }

            if cancelled_first {
                return AssistantMessageEventStream::from_error(AssistantMessage::error(
                    response.model_iden,
                    StopReason::Aborted,
                    "Request aborted by user",
                ));
            }

            let result = match peeked {
                // Handshake succeeded but the stream produced no event; finalize in-band exactly as
                // the normal stream loop would on an immediate upstream close.
                None => {
                    let mut accumulator = AssistantAccumulator::new(response.model_iden);
                    let pending: VecDeque<AssistantMessageEvent> =
                        accumulator.finish_without_end().into();
                    let finished = accumulator.is_terminal();
                    return finalize_event_stream(
                        response.stream,
                        accumulator,
                        cancel,
                        pending,
                        finished,
                        price_catalog,
                    );
                }
                Some(result) => result,
            };

            // Retry only a first-event, retryable HTTP handshake error with retries remaining.
            if let Err(ref error) = result
                && let Some((status, headers)) = extract_http_error(error)
                && attempt < max_retries
                && is_retryable_http_error(status, &headers)
            {
                match compute_retry_decision(
                    &headers,
                    &error.to_string(),
                    attempt,
                    max_retry_delay_ms,
                ) {
                    // Server-requested delay above the cap: fail immediately with pi's exact message.
                    RetryDecision::FailFast(message) => {
                        return AssistantMessageEventStream::from_error(AssistantMessage::error(
                            response.model_iden,
                            StopReason::Error,
                            message,
                        ));
                    }
                    // Cancellation during the retry sleep aborts in-band rather than sleeping on.
                    RetryDecision::Sleep(delay) => {
                        let slept = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => false,
                            _ = tokio::time::sleep(delay) => true,
                        };
                        if !slept {
                            return AssistantMessageEventStream::from_error(
                                AssistantMessage::error(
                                    response.model_iden,
                                    StopReason::Aborted,
                                    "Request aborted by user",
                                ),
                            );
                        }
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                }
            }

            // Real content, a non-retryable/parse error, an `x-should-retry: false` override, or
            // exhausted retries: pass the peeked event through and stream normally. Because a first
            // event has now been consumed, no later mid-stream failure is ever retried.
            let mut accumulator = AssistantAccumulator::new(response.model_iden);
            let mut pending: VecDeque<AssistantMessageEvent> = VecDeque::new();
            pending.extend(accumulator.fold_result(result));
            let finished = accumulator.is_terminal();
            return finalize_event_stream(
                response.stream,
                accumulator,
                cancel,
                pending,
                finished,
                price_catalog,
            );
        }
    }
}

/// Build the crate's assistant event stream from a genai upstream chat stream.
///
/// This is the shared tail of [`GenaiStreamFn::stream`]: it folds upstream items into assistant
/// events, ends every stream on a terminal event (`abort` on cancellation, `finish_without_end` on
/// an upstream close), and attaches monetary cost at finalization when a catalog is set. `pending`
/// and `finished` let a caller pre-seed the accumulator with an already-peeked first event so the
/// retry layer can inspect the handshake without losing it.
fn finalize_event_stream(
    upstream: ChatStream,
    accumulator: AssistantAccumulator,
    cancel: CancellationToken,
    pending: VecDeque<AssistantMessageEvent>,
    finished: bool,
    price_catalog: Option<Arc<dyn PriceCatalog>>,
) -> AssistantMessageEventStream {
    let state = (upstream, accumulator, cancel, pending, finished);
    let stream = futures::stream::unfold(
        state,
        |(mut upstream, mut accumulator, cancel, mut pending, mut finished)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((event, (upstream, accumulator, cancel, pending, finished)));
                }
                if finished {
                    return None;
                }

                let events = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => accumulator.abort(),
                    item = upstream.next() => match item {
                        Some(item) => accumulator.fold_result(item),
                        None => accumulator.finish_without_end(),
                    },
                };
                finished = accumulator.is_terminal();
                pending.extend(events);
            }
        },
    );

    // Attach monetary cost to the authoritative terminal message once its captured usage has
    // become an `AgentUsage`. Non-terminal snapshots and unpriced models are left untouched.
    let stream = stream.map(move |mut event| {
        if let Some(catalog) = price_catalog.as_deref() {
            attach_cost(catalog, &mut event);
        }
        event
    });

    AssistantMessageEventStream::from_stream(stream)
}

/// Outcome of computing the delay for a retryable handshake error.
enum RetryDecision {
    /// Sleep this long (cancellation-aware), then re-issue the request.
    Sleep(Duration),
    /// The server-requested delay exceeds the cap; fail immediately with this message.
    FailFast(String),
}

/// Downcast a first-event stream error to `(status, headers)` when it is a streaming HTTP handshake
/// failure.
///
/// The genai fork wraps a failed streaming handshake as [`genai::Error::WebStream`] whose boxed
/// inner error downcasts to [`genai::Error::HttpError`] (which now carries the response
/// [`HeaderMap`]); a directly-surfaced `HttpError` is accepted defensively. Any other error
/// (setup, parse, or a mid-stream provider error) returns `None` and is never retried.
fn extract_http_error(error: &genai::Error) -> Option<(StatusCode, HeaderMap)> {
    if let genai::Error::WebStream { error: inner, .. } = error
        && let Some(genai::Error::HttpError {
            status, headers, ..
        }) = inner.downcast_ref::<genai::Error>()
    {
        return Some((*status, (**headers).clone()));
    }
    if let genai::Error::HttpError {
        status, headers, ..
    } = error
    {
        return Some((*status, (**headers).clone()));
    }
    None
}

/// Classify a handshake HTTP error as retryable, mirroring pi-ai's `isRetryableProviderError`
/// (`provider-retry.ts:23-35`): honor `x-should-retry` first (a hard override in both directions),
/// then retry `408`, `409`, `429`, and any `>= 500`.
///
/// pi additionally retries when `error.status === undefined`; that case does not arise here because
/// an [`genai::Error::HttpError`] always carries a concrete status.
fn is_retryable_http_error(status: StatusCode, headers: &HeaderMap) -> bool {
    match header_value(headers, "x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    let code = status.as_u16();
    code == 408 || code == 409 || code == 429 || code >= 500
}

/// Read a header value as `&str`, or `None` when absent or non-ASCII.
fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Compute the retry delay for a retryable handshake error, mirroring pi-ai's `getRetryDelayMs` +
/// `validateServerRetryDelayMs` (`provider-retry.ts:37-67`).
///
/// Precedence: `retry-after-ms` (float milliseconds) > `retry-after` (float seconds or an
/// HTTP-date) > exponential backoff with jitter. A *server-requested* delay above the cap fails
/// fast with pi's exact message; the computed exponential backoff is never capped.
fn compute_retry_decision(
    headers: &HeaderMap,
    provider_error_message: &str,
    retry_index: u32,
    max_retry_delay_ms: u64,
) -> RetryDecision {
    // `retry-after-ms`: float milliseconds. pi only falls through when the value is absent/empty or
    // does not parse.
    if let Some(raw) = header_value(headers, "retry-after-ms").filter(|value| !value.is_empty())
        && let Ok(value) = raw.trim().parse::<f64>()
    {
        return validate_server_delay(value, max_retry_delay_ms, provider_error_message);
    }

    // `retry-after`: float seconds, or an HTTP-date (`Date.parse(retryAfter) - Date.now()`).
    if let Some(raw) = header_value(headers, "retry-after").filter(|value| !value.is_empty()) {
        let trimmed = raw.trim();
        let delay_ms = match trimmed.parse::<f64>() {
            Ok(seconds) => seconds * 1000.0,
            Err(_) => http_date_delay_ms(trimmed),
        };
        return validate_server_delay(delay_ms, max_retry_delay_ms, provider_error_message);
    }

    // Exponential backoff with jitter; never subject to the cap.
    let capped_secs =
        (RETRY_BASE_DELAY_SECS * 2f64.powi(retry_index as i32)).min(RETRY_MAX_BACKOFF_SECS);
    let delay_ms =
        capped_secs * 1000.0 * (1.0 - jitter_unit_interval(retry_index) * RETRY_JITTER_FRACTION);
    RetryDecision::Sleep(duration_from_millis_f64(delay_ms))
}

/// pi's `validateServerRetryDelayMs`: a server-requested delay above the cap fails fast; a cap of
/// `0` disables the limit. The requested/cap seconds are rendered with `Math.ceil`, matching pi's
/// `Server requested ${ceil}s retry delay (max: ${ceil}s). ${message}` byte for byte.
fn validate_server_delay(
    delay_ms: f64,
    max_retry_delay_ms: u64,
    provider_error_message: &str,
) -> RetryDecision {
    if max_retry_delay_ms > 0 && delay_ms > max_retry_delay_ms as f64 {
        let requested_secs = (delay_ms / 1000.0).ceil() as i64;
        let cap_secs = (max_retry_delay_ms as f64 / 1000.0).ceil() as i64;
        return RetryDecision::FailFast(format!(
            "Server requested {requested_secs}s retry delay (max: {cap_secs}s). {provider_error_message}"
        ));
    }
    RetryDecision::Sleep(duration_from_millis_f64(delay_ms))
}

/// Convert a millisecond delay to a [`Duration`], clamping to `>= 0` like pi's
/// `abortableSleep`'s `Math.max(0, ms)` (a `Duration` cannot be negative). A `NaN` delay (an
/// unparseable HTTP-date, mirroring `Date.parse` returning `NaN`) also clamps to zero. An
/// out-of-range delay (a malicious or buggy `retry-after` value can exceed `Duration`'s range)
/// safely saturates at `Duration::MAX` instead of panicking — the never-throw [`StreamFn`]
/// contract — and the cancellation-aware sleep still wins over the saturated wait.
fn duration_from_millis_f64(ms: f64) -> Duration {
    let secs = ms.max(0.0) / 1000.0;
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}

/// A jitter value in `[0, 1)` for pi's `1 - Math.random() * 0.25` backoff jitter, without pulling
/// in a `rand`/`fastrand` dependency. Seeds a xorshift64 from a cheap process-local entropy source
/// (wall-clock nanoseconds mixed with a monotonic counter and the retry index); the exact value is
/// unimportant, only that successive retries jitter differently.
fn jitter_unit_interval(retry_index: u32) -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x =
        nanos ^ seq.rotate_left(32) ^ u64::from(retry_index).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if x == 0 {
        x = 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    // Top 53 bits map exactly into an f64 mantissa, giving a value in [0, 1).
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Convert an HTTP-date `retry-after` value into a delay-from-now in milliseconds, mirroring pi's
/// `Date.parse(retryAfter) - Date.now()`. Returns `f64::NAN` for an unparseable value (pi's
/// `Date.parse` yields `NaN`, and a `NaN` delay sleeps ~immediately without tripping the cap).
fn http_date_delay_ms(value: &str) -> f64 {
    match parse_imf_fixdate(value) {
        Some(epoch_secs) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            epoch_secs as f64 * 1000.0 - now_ms
        }
        None => f64::NAN,
    }
}

/// Parse an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`) into seconds since the Unix epoch.
///
/// This is the format RFC 7231 §7.1.1.1 says senders SHOULD generate; the obsolete RFC 850 and
/// asctime forms are not accepted (an unparseable value is treated as an ~immediate retry, as pi's
/// `Date.parse` → `NaN` is). The trailing timezone token is assumed `GMT`.
fn parse_imf_fixdate(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month: i64 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since the Unix epoch for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Compute and attach monetary cost to a terminal assistant event's authoritative message.
///
/// This is the finalization hook applied by [`GenaiStreamFn`] when a price catalog is configured,
/// exposed so applications building custom [`StreamFn`] implementations can reuse the exact same
/// behavior. Non-terminal events are left unchanged. For a terminal event, the catalog is queried
/// for the message's model: when the model is priced, [`crate::AgentUsage::cost`] is set via
/// [`compute_cost`]; when it is not, the cost stays `None`.
pub fn attach_cost(catalog: &dyn PriceCatalog, event: &mut AssistantMessageEvent) {
    let message = match event {
        AssistantMessageEvent::Done { message, .. } => message,
        AssistantMessageEvent::Error { error, .. } => error,
        _ => return,
    };
    if let Some(model_cost) = catalog.cost_model(&message.model) {
        message.usage.cost = Some(compute_cost(&message.usage, &model_cost));
    }
}

/// Adapt a crate-level [`OnPayloadHook`] into the genai fork's client-level
/// [`PayloadInterceptor`].
///
/// The returned interceptor delegates every call to the hook, forwarding the serialized provider
/// payload and target [`ModelIden`] unchanged; `Some` replaces the wire payload and `None` keeps
/// it. [`GenaiStreamFn::with_exec_hooks`] installs this adapter for you; it is public so
/// applications building their own `genai` [`ClientBuilder`] can install the exact same
/// delegation with [`ClientBuilder::with_payload_interceptor`].
pub fn payload_interceptor_from_hook(hook: OnPayloadHook) -> PayloadInterceptor {
    PayloadInterceptor::from_interceptor_async_fn(
        move |model_iden: ModelIden, payload: Value| -> BoxFuture<'static, Option<Value>> {
            hook(payload, model_iden)
        },
    )
}

/// Adapt a crate-level [`OnResponseHook`] into the genai fork's client-level [`ResponseObserver`].
///
/// The returned observer converts the response head into a [`StreamResponseInfo`] (via
/// [`header_pairs`]) and delegates to the hook with the target [`ModelIden`].
/// [`GenaiStreamFn::with_exec_hooks`] installs this adapter for you; it is public so applications
/// building their own `genai` [`ClientBuilder`] can install the exact same delegation with
/// [`ClientBuilder::with_response_observer`].
pub fn response_observer_from_hook(hook: OnResponseHook) -> ResponseObserver {
    ResponseObserver::from_observer_async_fn(
        move |model_iden: ModelIden,
              status: StatusCode,
              headers: HeaderMap|
              -> BoxFuture<'static, ()> {
            let info = StreamResponseInfo::new(status.as_u16(), header_pairs(&headers));
            hook(info, model_iden)
        },
    )
}

/// Convert a response [`HeaderMap`] into the owned `(name, value)` pairs carried by
/// [`StreamResponseInfo`].
///
/// Header names are reqwest-normalized (lowercase), a repeated header contributes one pair per
/// value, and non-UTF-8 header values are lossily converted. Exposed so custom [`StreamFn`]
/// implementations invoking an [`OnResponseHook`] build the same shape the built-in stream
/// functions produce.
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn force_capture_options(mut options: ChatOptions) -> ChatOptions {
    options.capture_content = Some(true);
    options.capture_tool_calls = Some(true);
    options.capture_usage = Some(true);
    options.capture_reasoning_content = Some(true);
    options
}

/// Apply invocation options over the adapter's base options. `ChatOptions` uses `Option` for every
/// scalar, so `None` naturally means "inherit". An empty stop list likewise inherits the base list;
/// callers can construct a second adapter when they need to clear a non-empty inherited list.
fn overlay_chat_options(base: &ChatOptions, request: ChatOptions) -> ChatOptions {
    let mut merged = base.clone();

    macro_rules! overlay {
        ($field:ident) => {
            if request.$field.is_some() {
                merged.$field = request.$field;
            }
        };
    }

    overlay!(temperature);
    overlay!(max_tokens);
    overlay!(top_p);
    if !request.stop_sequences.is_empty() {
        merged.stop_sequences = request.stop_sequences;
    }
    overlay!(capture_usage);
    overlay!(capture_content);
    overlay!(capture_reasoning_content);
    overlay!(capture_tool_calls);
    overlay!(capture_raw_body);
    overlay!(response_format);
    overlay!(tool_choice);
    overlay!(normalize_reasoning_content);
    overlay!(reasoning_effort);
    overlay!(verbosity);
    overlay!(seed);
    overlay!(service_tier);
    overlay!(extra_headers);
    overlay!(cache_control);
    overlay!(prompt_cache_key);
    overlay!(extra_body);

    merged
}

static DEFAULT_STREAM_FN: OnceLock<RwLock<Option<Arc<dyn StreamFn>>>> = OnceLock::new();

fn default_slot() -> &'static RwLock<Option<Arc<dyn StreamFn>>> {
    DEFAULT_STREAM_FN.get_or_init(|| RwLock::new(None))
}

/// Atomically install, replace, or clear the process-global fallback stream function.
///
/// An explicitly configured stream function on an agent or low-level run takes precedence. Runs
/// that need the fallback clone the value when they are admitted, so replacing it affects later
/// admissions rather than an invocation already in progress.
///
/// The returned previous value supports scoped replacement and restoration. Because the slot is
/// shared by every thread and agent in the process, concurrent tests should serialize changes and
/// restore the previous value when finished.
pub fn set_default_stream_fn(stream_fn: Option<Arc<dyn StreamFn>>) -> Option<Arc<dyn StreamFn>> {
    std::mem::replace(
        &mut *default_slot()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        stream_fn,
    )
}

/// Clone the process-global fallback for internal run admission.
pub(crate) fn get_default_stream_fn() -> Option<Arc<dyn StreamFn>> {
    default_slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn model_iden_for_error(model: &ModelSpec) -> ModelIden {
    match model {
        ModelSpec::Iden(model) => model.clone(),
        ModelSpec::Target(target) => target.model.clone(),
        ModelSpec::Name(name) => {
            let name = name.to_string();
            ModelIden::new(
                AdapterKind::from_model(&name).unwrap_or(AdapterKind::Ollama),
                name,
            )
        }
    }
}

impl Default for GenaiStreamFn {
    fn default() -> Self {
        Self::new(Client::default())
    }
}

#[cfg(feature = "testing")]
pub(crate) fn stream_error_model(model: &ModelSpec) -> ModelIden {
    let model = model_iden_for_error(model);
    if model.model_name.is_empty() {
        crate::assistant::unknown_model_iden()
    } else {
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AgentUsage, AssistantContent, AssistantMessage, ModelCost, ModelCostRates, StopReason,
    };
    use futures::StreamExt;
    use genai::ModelIden;
    use genai::adapter::AdapterKind;

    /// Price catalog that prices exactly one model name.
    struct SingleModelCatalog {
        model_name: String,
        cost: ModelCost,
    }

    impl PriceCatalog for SingleModelCatalog {
        fn cost_model(&self, model: &ModelIden) -> Option<ModelCost> {
            (model.model_name.as_str() == self.model_name).then(|| self.cost.clone())
        }
    }

    fn priced_catalog(model_name: &str) -> SingleModelCatalog {
        SingleModelCatalog {
            model_name: model_name.to_owned(),
            cost: ModelCost::new(ModelCostRates {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
                cache_write_1h: None,
            }),
        }
    }

    fn done_event(model: ModelIden, usage: AgentUsage) -> AssistantMessageEvent {
        let mut message = AssistantMessage::completed(
            model,
            vec![AssistantContent::text("ok")],
            StopReason::Stop,
        );
        message.usage = usage;
        AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            message,
        }
    }

    fn one_m_in_one_m_out() -> AgentUsage {
        AgentUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..AgentUsage::default()
        }
    }

    #[test]
    fn attach_cost_sets_cost_on_priced_terminal_message() {
        let model = ModelIden::new(AdapterKind::Anthropic, "claude-test");
        let catalog = priced_catalog("claude-test");
        let mut event = done_event(model, one_m_in_one_m_out());

        attach_cost(&catalog, &mut event);

        let cost = event
            .terminal_message()
            .unwrap()
            .usage
            .cost
            .expect("cost attached");
        // 1M input at $3/M + 1M output at $15/M.
        assert!((cost.total - 18.0).abs() < 1e-9);
    }

    #[test]
    fn attach_cost_leaves_unpriced_model_cost_none() {
        let model = ModelIden::new(AdapterKind::Anthropic, "some-other-model");
        let catalog = priced_catalog("claude-test");
        let mut event = done_event(model, one_m_in_one_m_out());

        attach_cost(&catalog, &mut event);

        assert!(event.terminal_message().unwrap().usage.cost.is_none());
    }

    #[test]
    fn attach_cost_ignores_non_terminal_events() {
        let model = ModelIden::new(AdapterKind::Anthropic, "claude-test");
        let catalog = priced_catalog("claude-test");
        let mut event = AssistantMessageEvent::Start {
            partial: AssistantMessage::new(model),
        };

        attach_cost(&catalog, &mut event);

        assert!(event.partial().usage.cost.is_none());
    }

    #[test]
    fn header_pairs_normalizes_names_and_lossily_converts_values() {
        let mut headers = HeaderMap::new();
        headers.append("X-Repeated", "one".parse().unwrap());
        headers.append("x-repeated", "two".parse().unwrap());
        headers.insert(
            "x-binary",
            reqwest::header::HeaderValue::from_bytes(&[0x66, 0xFF, 0x6F]).unwrap(),
        );

        let pairs = header_pairs(&headers);

        // reqwest normalizes header names to lowercase; repeats keep one pair per value.
        assert_eq!(
            pairs
                .iter()
                .filter(|(name, _)| name == "x-repeated")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        // Non-UTF-8 header values are lossily converted rather than dropped.
        assert_eq!(
            pairs
                .iter()
                .find(|(name, _)| name == "x-binary")
                .map(|(_, value)| value.as_str()),
            Some("f\u{FFFD}o")
        );
    }

    #[test]
    fn stream_response_info_header_lookup_is_case_insensitive() {
        let info = StreamResponseInfo::new(
            429,
            vec![
                ("retry-after".to_owned(), "2".to_owned()),
                ("retry-after".to_owned(), "9".to_owned()),
            ],
        );
        assert_eq!(info.status, 429);
        // First value wins; lookup ignores ASCII case.
        assert_eq!(info.header("Retry-After"), Some("2"));
        assert_eq!(info.header("x-missing"), None);
    }

    /// Exercises the exact finalization wiring `GenaiStreamFn::stream` uses: `attach_cost` mapped
    /// over the raw event stream before it is wrapped, so the published terminal message carries
    /// cost. The full method needs a live `genai` client, so this covers the seam's cost path
    /// without one.
    #[tokio::test]
    async fn cost_attaches_through_stream_finalization_wiring() {
        let model = ModelIden::new(AdapterKind::Anthropic, "claude-test");
        let catalog: Arc<dyn PriceCatalog> = Arc::new(priced_catalog("claude-test"));
        let events = vec![
            AssistantMessageEvent::Start {
                partial: AssistantMessage::new(model.clone()),
            },
            done_event(model, one_m_in_one_m_out()),
        ];

        let catalog_for_map = catalog.clone();
        let mapped = futures::stream::iter(events).map(move |mut event| {
            attach_cost(catalog_for_map.as_ref(), &mut event);
            event
        });
        let mut stream = AssistantMessageEventStream::from_stream(mapped);
        let result = stream.result_handle();
        while stream.next().await.is_some() {}

        let message = result.get().await.expect("terminal message");
        let cost = message.usage.cost.expect("cost attached at finalization");
        assert!((cost.total - 18.0).abs() < 1e-9);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn sleep_ms(decision: RetryDecision) -> f64 {
        match decision {
            RetryDecision::Sleep(duration) => duration.as_secs_f64() * 1000.0,
            RetryDecision::FailFast(message) => panic!("expected Sleep, got FailFast: {message}"),
        }
    }

    #[test]
    fn retry_policy_defaults_mirror_pi() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 0);
        assert_eq!(policy.max_retry_delay_ms, 60_000);
    }

    #[test]
    fn stream_request_session_and_retry_fields_default_to_none() {
        let request = StreamRequest::new(
            ModelSpec::from_iden(ModelIden::new(AdapterKind::Anthropic, "claude-test")),
            LlmContext::default(),
        );
        assert_eq!(request.session_id, None);
        assert_eq!(request.max_retries, None);
        assert_eq!(request.max_retry_delay_ms, None);
        assert_eq!(request.options.prompt_cache_key, None);
    }

    #[test]
    fn stream_request_debug_shows_scalar_fields_and_redacts_closures() {
        let request = StreamRequest::new(
            ModelSpec::from_iden(ModelIden::new(AdapterKind::Anthropic, "claude-test")),
            LlmContext::default(),
        )
        .with_session_id("req-session")
        .with_max_retries(2)
        .with_max_retry_delay_ms(500)
        .with_on_payload(Arc::new(|payload, _model| {
            Box::pin(async move { Some(payload) })
        }));

        let debug = format!("{request:?}");
        assert!(
            debug.contains("session_id: Some(\"req-session\")"),
            "{debug}"
        );
        assert!(debug.contains("max_retries: Some(2)"), "{debug}");
        assert!(debug.contains("max_retry_delay_ms: Some(500)"), "{debug}");
        assert!(debug.contains("on_payload: true"), "{debug}");
        // The session id never leaks into the options' prompt cache key.
        assert_eq!(request.options.prompt_cache_key, None);
    }

    #[test]
    fn retryable_statuses_match_pi_policy() {
        let empty = headers(&[]);
        for code in [408_u16, 409, 429, 500, 502, 503, 599] {
            assert!(
                is_retryable_http_error(StatusCode::from_u16(code).unwrap(), &empty),
                "{code} should be retryable"
            );
        }
        for code in [400_u16, 401, 403, 404, 422] {
            assert!(
                !is_retryable_http_error(StatusCode::from_u16(code).unwrap(), &empty),
                "{code} should not be retryable"
            );
        }
    }

    #[test]
    fn x_should_retry_overrides_status_in_both_directions() {
        // `true` forces a retry even on an otherwise non-retryable status.
        assert!(is_retryable_http_error(
            StatusCode::BAD_REQUEST,
            &headers(&[("x-should-retry", "true")]),
        ));
        // `false` is a hard no-retry override even on an otherwise-retryable status.
        assert!(!is_retryable_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("x-should-retry", "false")]),
        ));
    }

    #[test]
    fn retry_after_ms_header_takes_precedence() {
        let decision = compute_retry_decision(
            &headers(&[("retry-after-ms", "20"), ("retry-after", "99")]),
            "boom",
            0,
            60_000,
        );
        assert!((sleep_ms(decision) - 20.0).abs() < 0.5);
    }

    #[test]
    fn retry_after_seconds_header_is_used_when_ms_absent() {
        let decision = compute_retry_decision(&headers(&[("retry-after", "2")]), "boom", 0, 60_000);
        assert!((sleep_ms(decision) - 2000.0).abs() < 0.5);
    }

    #[test]
    fn non_numeric_retry_after_ms_falls_through_to_retry_after() {
        // pi only returns from the `retry-after-ms` branch when it parses; otherwise it falls
        // through to `retry-after`.
        let decision = compute_retry_decision(
            &headers(&[("retry-after-ms", "not-a-number"), ("retry-after", "3")]),
            "boom",
            0,
            60_000,
        );
        assert!((sleep_ms(decision) - 3000.0).abs() < 0.5);
    }

    #[test]
    fn server_delay_above_cap_fails_fast_with_pi_exact_message() {
        // retry-after-ms: 1500 (1.5s) with a 1000ms (1s) cap. pi renders both bounds with
        // `Math.ceil`, so 1.5 -> 2 and 1.0 -> 1.
        let decision = compute_retry_decision(
            &headers(&[("retry-after-ms", "1500")]),
            "provider says no",
            0,
            1000,
        );
        match decision {
            RetryDecision::FailFast(message) => assert_eq!(
                message,
                "Server requested 2s retry delay (max: 1s). provider says no"
            ),
            RetryDecision::Sleep(_) => panic!("expected FailFast"),
        }
    }

    #[test]
    fn zero_cap_disables_the_server_delay_limit() {
        // A huge server-requested delay with the cap disabled sleeps rather than failing fast.
        let decision = compute_retry_decision(&headers(&[("retry-after", "3600")]), "boom", 0, 0);
        assert!((sleep_ms(decision) - 3_600_000.0).abs() < 1.0);
    }

    #[test]
    fn exponential_backoff_is_never_capped_and_stays_within_jitter_bounds() {
        // No retry-after headers -> exponential path. retry_index 0 -> base 500ms, jittered into
        // (375, 500]. A tiny cap must NOT apply to the computed backoff.
        for _ in 0..64 {
            let delay = sleep_ms(compute_retry_decision(&headers(&[]), "boom", 0, 1));
            assert!(
                delay > 374.0 && delay <= 500.0001,
                "delay {delay} out of bounds"
            );
        }
        // retry_index 5 -> base min(0.5*2^5, 8) = 8s, jittered into (6000, 8000]; still uncapped.
        let delay = sleep_ms(compute_retry_decision(&headers(&[]), "boom", 5, 1));
        assert!(
            delay > 5999.0 && delay <= 8000.0001,
            "delay {delay} out of bounds"
        );
    }

    #[test]
    fn jitter_stays_in_unit_interval() {
        for index in 0..1000 {
            let jitter = jitter_unit_interval(index % 8);
            assert!(
                (0.0..1.0).contains(&jitter),
                "jitter {jitter} out of [0, 1)"
            );
        }
    }

    #[test]
    fn imf_fixdate_parses_to_epoch_seconds() {
        // Date.parse("Wed, 21 Oct 2015 07:28:00 GMT") === 1445412480000 ms.
        assert_eq!(
            parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        // The Unix epoch itself.
        assert_eq!(parse_imf_fixdate("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // Unparseable values yield None (treated as an ~immediate retry, like Date.parse -> NaN).
        assert_eq!(parse_imf_fixdate("not a date"), None);
    }
}
