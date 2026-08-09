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
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, Tool};
use genai::{Client, ClientBuilder, ModelIden, ModelSpec, PayloadInterceptor, ResponseObserver};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock, RwLock};
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
    /// The agent loop forwards the configured hook here so custom [`StreamFn`] implementations
    /// and the `proxy`-feature `ProxyStreamFn` can honor it per request. The production
    /// [`GenaiStreamFn`] ignores this field: the genai fork's payload interceptor is
    /// client-level, so it must be installed at construction time via
    /// [`GenaiStreamFn::with_exec_hooks`] (pass the same hook in both places).
    pub on_payload: Option<OnPayloadHook>,
    /// Optional response observation hook for this invocation (pi's `onResponse`).
    ///
    /// The agent loop forwards the configured hook here so custom [`StreamFn`] implementations
    /// and the `proxy`-feature `ProxyStreamFn` can honor it per request. The production
    /// [`GenaiStreamFn`] ignores this field: the genai fork's response observer is client-level,
    /// so it must be installed at construction time via [`GenaiStreamFn::with_exec_hooks`] (pass
    /// the same hook in both places).
    pub on_response: Option<OnResponseHook>,
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

/// `genai::Client` adapter for the provider-neutral assistant event protocol.
///
/// Capture options are forced on every invocation because `StreamEnd` is the authoritative source
/// for final content, parsed tool arguments, usage, stop reason, and response id.
///
/// `on_payload`/`on_response` exec hooks are construction-time only for this adapter (see
/// [`GenaiStreamFn::with_exec_hooks`]); the per-request [`StreamRequest`] hook fields are
/// intentionally ignored because the genai fork's interceptors are client-level.
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
}

impl std::fmt::Debug for GenaiStreamFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiStreamFn")
            .field("client", &self.client)
            .field("base_options", &self.base_options)
            .field("price_catalog", &self.price_catalog.is_some())
            .finish()
    }
}

impl GenaiStreamFn {
    /// Construct an adapter with default base chat options and no price catalog.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_options: ChatOptions::default(),
            price_catalog: None,
        }
    }

    /// Construct an adapter whose `genai` client is built from `builder` with construction-time
    /// `on_payload`/`on_response` exec hooks installed.
    ///
    /// The genai fork's exec hooks ([`PayloadInterceptor`] / [`ResponseObserver`]) are
    /// **client-level**, and a built [`Client`] cannot be reconfigured, so this adapter wires the
    /// crate-level hooks once at construction — mirroring how pi wires `onPayload`/`onResponse`
    /// once at its production `Agent` construction site. Consequently [`GenaiStreamFn`] ignores
    /// the per-request [`StreamRequest::on_payload`]/[`StreamRequest::on_response`] fields;
    /// applications that also want custom stream functions or the proxy to see the hooks should
    /// pass the same hooks both here and on [`crate::AgentConfig`].
    ///
    /// `on_payload` fires with the serialized provider payload before the HTTP request is built
    /// (`Some` replaces the wire payload). `on_response` fires with the response status and
    /// headers as soon as the HTTP response arrives — before its body/stream is consumed and also
    /// on 4xx/5xx. On genai's streaming path the HTTP send is lazy, so both fire during the first
    /// stream poll rather than at [`StreamFn::stream`] return time.
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
            // The genai fork's exec hooks are client-level, so the per-request hooks are
            // intentionally ignored here; install them at construction time via
            // `GenaiStreamFn::with_exec_hooks` (see the `StreamRequest` field docs).
            on_payload: _,
            on_response: _,
            cancel,
        } = request;
        let error_model = model_iden_for_error(&model);
        let chat_request = context.into_chat_request();
        let options = force_capture_options(overlay_chat_options(&self.base_options, options));

        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return AssistantMessageEventStream::from_error(AssistantMessage::error(
                    error_model,
                    StopReason::Aborted,
                    "Request aborted by user",
                ));
            }
            response = self.client.exec_chat_stream(model, chat_request, Some(&options)) => response,
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
        let state = (response.stream, accumulator, cancel, VecDeque::new(), false);
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
        let price_catalog = self.price_catalog.clone();
        let stream = stream.map(move |mut event| {
            if let Some(catalog) = price_catalog.as_deref() {
                attach_cost(catalog, &mut event);
            }
            event
        });

        AssistantMessageEventStream::from_stream(stream)
    }
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
}
