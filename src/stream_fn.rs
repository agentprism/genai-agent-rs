//! Injectable provider streaming boundary and its process-global fallback.
//!
//! [`StreamFn`] is deliberately a never-throw interface: setup failures, stream failures, and
//! cancellation are represented by terminal assistant events. [`GenaiStreamFn`] provides the
//! production adapter, while [`set_default_stream_fn`] configures the fallback shared by all agents
//! in the process.

use crate::{
    AssistantAccumulator, AssistantMessage, AssistantMessageEventStream, StopReason, Transport,
};
use async_trait::async_trait;
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, Tool};
use genai::{Client, ModelIden, ModelSpec};
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
#[derive(Debug, Clone)]
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
    /// Cooperative cancellation token for setup and streaming.
    pub cancel: CancellationToken,
}

impl StreamRequest {
    /// Construct a request with default chat options, the default transport, and a fresh
    /// cancellation token.
    pub fn new(model: impl Into<ModelSpec>, context: LlmContext) -> Self {
        Self {
            model: model.into(),
            context,
            options: ChatOptions::default(),
            transport: Transport::default(),
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
#[derive(Debug, Clone)]
pub struct GenaiStreamFn {
    /// `genai` client used for provider execution.
    pub client: Client,
    /// Options inherited by invocations unless their corresponding request field overrides them.
    pub base_options: ChatOptions,
}

impl GenaiStreamFn {
    /// Construct an adapter with default base chat options.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_options: ChatOptions::default(),
        }
    }

    /// Replace the options inherited by future invocations.
    ///
    /// Per-request `Some` fields override these values; required final-content, tool-call, usage,
    /// and reasoning capture is forced on after option merging.
    pub fn with_base_options(mut self, options: ChatOptions) -> Self {
        self.base_options = options;
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

        AssistantMessageEventStream::from_stream(stream)
    }
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
