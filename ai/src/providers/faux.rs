use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::auth::types::{ApiKeyAuth, AuthResult, ProviderAuth};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, DeferredCancelOptions,
    DeferredFetchOptions, DeferredHandle, DeferredRequest, ErrorStopReason, ImageContent, Message,
    Model, ModelCost, ModelCostRates, ModelInput, ProviderResponse, SimpleStreamOptions,
    StopReason, SuccessfulStopReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage,
    Usage, UsageCost, UserContent, UserContentBlock,
};
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_API: &str = "faux";
const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";
const DEFAULT_MODEL_NAME: &str = "Faux Model";
const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

#[derive(Debug, Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<ModelInput>>,
    pub cost: Option<ModelCostRates>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

impl FauxModelDefinition {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: None,
            max_tokens: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum FauxAssistantContent {
    Text(String),
    Block(AssistantContent),
    Blocks(Vec<AssistantContent>),
}

impl From<&str> for FauxAssistantContent {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for FauxAssistantContent {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<AssistantContent> for FauxAssistantContent {
    fn from(value: AssistantContent) -> Self {
        Self::Block(value)
    }
}

impl From<Vec<AssistantContent>> for FauxAssistantContent {
    fn from(value: Vec<AssistantContent>) -> Self {
        Self::Blocks(value)
    }
}

#[derive(Debug, Clone, Default)]
pub struct FauxAssistantMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub response_id: Option<String>,
    pub timestamp: Option<i64>,
}

pub fn faux_text(text: impl Into<String>) -> AssistantContent {
    AssistantContent::Text(TextContent::new(text))
}

pub fn faux_thinking(thinking: impl Into<String>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent::new(thinking))
}

pub fn faux_tool_call(
    name: impl Into<String>,
    arguments: impl Into<Value>,
    id: Option<String>,
) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::new(
        id.unwrap_or_else(|| random_id("tool")),
        name,
        arguments,
    ))
}

pub fn faux_assistant_message(
    content: impl Into<FauxAssistantContent>,
    options: FauxAssistantMessageOptions,
) -> AssistantMessage {
    let content = match content.into() {
        FauxAssistantContent::Text(text) => vec![faux_text(text)],
        FauxAssistantContent::Block(block) => vec![block],
        FauxAssistantContent::Blocks(blocks) => blocks,
    };
    let mut message = AssistantMessage::pending(
        DEFAULT_API,
        DEFAULT_PROVIDER,
        DEFAULT_MODEL_ID,
        options.timestamp.unwrap_or_else(now_ms),
    );
    message.content = content.into();
    message.usage = default_usage();
    message.stop_reason = options.stop_reason.unwrap_or(StopReason::Stop);
    message.deferred = options.deferred;
    message.error_message = options.error_message;
    message.response_id = options.response_id;
    message
}

#[derive(Debug, Default)]
pub struct FauxProviderState {
    call_count: AtomicU64,
    deferred_fetch_count: AtomicU64,
    cancelled_deferred: Mutex<Vec<DeferredHandle>>,
}

impl FauxProviderState {
    pub fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }

    pub fn deferred_fetch_count(&self) -> u64 {
        self.deferred_fetch_count.load(Ordering::SeqCst)
    }

    pub fn cancelled_deferred(&self) -> Vec<DeferredHandle> {
        self.cancelled_deferred
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub type FauxResponseFactory = Arc<
    dyn Fn(
            Context,
            Option<SimpleStreamOptions>,
            Arc<FauxProviderState>,
            Model,
        ) -> BoxFuture<'static, Result<AssistantMessage, String>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(FauxResponseFactory),
}

impl From<AssistantMessage> for FauxResponseStep {
    fn from(value: AssistantMessage) -> Self {
        Self::Message(Box::new(value))
    }
}

#[derive(Debug, Clone, Default)]
pub struct FauxDeferredOptions {
    pub pending_fetches: Option<u64>,
    pub poll_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxTokenSizeOptions {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterFauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Option<Vec<FauxModelDefinition>>,
    pub deferred: Option<FauxDeferredOptions>,
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<FauxTokenSizeOptions>,
}

#[derive(Clone)]
struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: Context,
    options: Option<SimpleStreamOptions>,
    model: Model,
    pending_fetches: u64,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

struct FauxCoreInner {
    api: String,
    provider: String,
    models: Vec<Model>,
    pending_responses: Mutex<VecDeque<FauxResponseStep>>,
    tokens_per_second: Option<f64>,
    min_token_size: usize,
    max_token_size: usize,
    deferred_options: FauxDeferredOptions,
    state: Arc<FauxProviderState>,
    prompt_cache: Mutex<HashMap<String, String>>,
    deferred_responses: Mutex<HashMap<String, DeferredEntry>>,
}

#[derive(Clone)]
pub struct FauxCore {
    inner: Arc<FauxCoreInner>,
}

impl FauxCore {
    pub fn api(&self) -> &str {
        &self.inner.api
    }

    pub fn provider_id(&self) -> &str {
        &self.inner.provider
    }

    pub fn models(&self) -> &[Model] {
        &self.inner.models
    }

    pub fn get_model(&self, model_id: Option<&str>) -> Option<Model> {
        model_id
            .filter(|model_id| !model_id.is_empty())
            .map_or_else(
                || self.inner.models.first().cloned(),
                |model_id| {
                    self.inner
                        .models
                        .iter()
                        .find(|model| model.id == model_id)
                        .cloned()
                },
            )
    }

    pub fn state(&self) -> Arc<FauxProviderState> {
        self.inner.state.clone()
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        *self
            .inner
            .pending_responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = responses.into();
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.inner
            .pending_responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(responses);
    }

    pub fn pending_response_count(&self) -> usize {
        self.inner
            .pending_responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn stream_response(
        &self,
        request_model: &Model,
        context: &Context,
        stream_options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = AssistantMessageEventStream::channel();
        let step = self
            .inner
            .pending_responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        self.inner.state.call_count.fetch_add(1, Ordering::SeqCst);
        let core = self.clone();
        let request_model = request_model.clone();
        let context = context.clone();
        tokio::spawn(async move {
            if let Err(error) = core
                .run_stream(
                    &sender,
                    step,
                    request_model.clone(),
                    context,
                    stream_options,
                )
                .await
            {
                let message = core.create_error_message(error, &request_model);
                let _ = sender.send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Error,
                    error: message,
                });
            }
        });
        stream
    }

    async fn run_stream(
        &self,
        sender: &AssistantStreamSender,
        step: Option<FauxResponseStep>,
        request_model: Model,
        context: Context,
        stream_options: Option<SimpleStreamOptions>,
    ) -> Result<(), String> {
        call_on_response(
            stream_options
                .as_ref()
                .and_then(|options| options.stream.request.on_response.clone()),
            &request_model,
        )
        .await?;
        let Some(step) = step else {
            let mut message = self
                .create_error_message("No more faux responses queued".to_owned(), &request_model);
            message = self.with_usage_estimate(message, &context, stream_options.as_ref());
            sender
                .send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Error,
                    error: message,
                })
                .map_err(|error| error.to_string())?;
            return Ok(());
        };

        if stream_options
            .as_ref()
            .and_then(|options| options.deferred.as_ref())
            .is_some_and(deferred_enabled)
        {
            let handle = DeferredHandle {
                provider: request_model.provider.0.clone(),
                model_id: request_model.id.clone(),
                api: request_model.api.0.clone(),
                id: random_id("deferred"),
                expires_at: None,
                poll_after_ms: self.inner.deferred_options.poll_after_ms,
                data: None,
            };
            self.inner
                .deferred_responses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    handle.id.clone(),
                    DeferredEntry {
                        handle: handle.clone(),
                        step,
                        context,
                        options: stream_options.clone(),
                        model: request_model.clone(),
                        pending_fetches: self
                            .inner
                            .deferred_options
                            .pending_fetches
                            .unwrap_or_default(),
                        cancelled: false,
                        final_message: None,
                    },
                );
            let message = create_deferred_message(&request_model, handle);
            return self
                .stream_with_deltas(sender, message, signal_from_simple(stream_options.as_ref()))
                .await;
        }

        let message = self
            .resolve_response(step, context, stream_options.clone(), request_model)
            .await?;
        self.stream_with_deltas(sender, message, signal_from_simple(stream_options.as_ref()))
            .await
    }

    async fn resolve_response(
        &self,
        step: FauxResponseStep,
        context: Context,
        stream_options: Option<SimpleStreamOptions>,
        request_model: Model,
    ) -> Result<AssistantMessage, String> {
        let resolved = match step {
            FauxResponseStep::Message(message) => *message,
            FauxResponseStep::Factory(factory) => {
                factory(
                    context.clone(),
                    stream_options.clone(),
                    self.inner.state.clone(),
                    request_model.clone(),
                )
                .await?
            }
        };
        let message = clone_message(
            resolved,
            &self.inner.api,
            &self.inner.provider,
            &request_model.id,
        );
        Ok(self.with_usage_estimate(message, &context, stream_options.as_ref()))
    }

    fn with_usage_estimate(
        &self,
        mut message: AssistantMessage,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessage {
        let prompt_text = serialize_context(context);
        let prompt_tokens = estimate_tokens(&prompt_text);
        let output_tokens = estimate_tokens(&assistant_content_to_text(&message.content));
        let mut input = prompt_tokens;
        let mut cache_read = 0;
        let mut cache_write = 0;

        if let Some(session_id) = options
            .and_then(|options| options.stream.session_id.as_deref())
            .filter(|_| {
                options.and_then(|options| options.stream.cache_retention)
                    != Some(CacheRetention::None)
            })
        {
            let mut prompt_cache = self
                .inner
                .prompt_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(previous) = prompt_cache.get(session_id) {
                let cached_units = common_prefix_length(previous, &prompt_text);
                cache_read = (cached_units as u64).div_ceil(4);
                cache_write = (prompt_text
                    .encode_utf16()
                    .count()
                    .saturating_sub(cached_units) as u64)
                    .div_ceil(4);
                input = prompt_tokens.saturating_sub(cache_read);
            } else {
                cache_write = prompt_tokens;
            }
            prompt_cache.insert(session_id.to_owned(), prompt_text);
        }

        message.usage = Usage {
            input: input.into(),
            output: output_tokens.into(),
            cache_read: cache_read.into(),
            cache_write: cache_write.into(),
            cache_write_1h: None,
            reasoning: None,
            total_tokens: (input + output_tokens + cache_read + cache_write).into(),
            cost: UsageCost::default(),
        };
        message
    }

    async fn stream_with_deltas(
        &self,
        sender: &AssistantStreamSender,
        message: AssistantMessage,
        signal: Option<Arc<dyn crate::types::AbortSignal>>,
    ) -> Result<(), String> {
        let mut partial = message.clone();
        partial.content = Vec::new().into();
        partial.stop_reason = StopReason::Pending;
        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
            let aborted = create_aborted_message(partial);
            sender
                .send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Aborted,
                    error: aborted,
                })
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        sender
            .send(AssistantMessageEvent::Start)
            .map_err(|error| error.to_string())?;

        for (index, block) in message.content.iter().cloned().enumerate() {
            if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                return send_aborted(sender, partial);
            }
            match block {
                AssistantContent::Thinking(block) => {
                    partial
                        .content
                        .push(AssistantContent::Thinking(ThinkingContent::new("")));
                    sender
                        .send(AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                            thinking: None,
                            thinking_signature: None,
                            redacted: None,
                        })
                        .map_err(|error| error.to_string())?;
                    for chunk in split_string_by_token_size(
                        &block.thinking,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                    ) {
                        schedule_chunk(&chunk, self.inner.tokens_per_second).await;
                        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                            return send_aborted(sender, partial);
                        }
                        if let AssistantContent::Thinking(content) = &mut partial.content[index] {
                            content.thinking.push_str(&chunk);
                        }
                        sender
                            .send(AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta: chunk,
                                thinking_signature_delta: None,
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    sender
                        .send(AssistantMessageEvent::ThinkingEnd {
                            content_index: index,
                            content: block.thinking,
                            content_signature: block.thinking_signature,
                            redacted: block.redacted,
                        })
                        .map_err(|error| error.to_string())?;
                }
                AssistantContent::Text(block) => {
                    partial
                        .content
                        .push(AssistantContent::Text(TextContent::new("")));
                    sender
                        .send(AssistantMessageEvent::TextStart {
                            content_index: index,
                        })
                        .map_err(|error| error.to_string())?;
                    for chunk in split_string_by_token_size(
                        &block.text,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                    ) {
                        schedule_chunk(&chunk, self.inner.tokens_per_second).await;
                        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                            return send_aborted(sender, partial);
                        }
                        if let AssistantContent::Text(content) = &mut partial.content[index] {
                            content.text.push_str(&chunk);
                        }
                        sender
                            .send(AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta: chunk,
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    sender
                        .send(AssistantMessageEvent::TextEnd {
                            content_index: index,
                            content: block.text,
                            content_signature: block.text_signature,
                        })
                        .map_err(|error| error.to_string())?;
                }
                AssistantContent::ToolCall(block) => {
                    partial
                        .content
                        .push(AssistantContent::ToolCall(ToolCall::new(
                            block.id.clone(),
                            block.name.clone(),
                            Value::Object(Default::default()),
                        )));
                    sender
                        .send(AssistantMessageEvent::ToolCallStart {
                            content_index: index,
                            id: block.id.clone(),
                            tool_name: block.name.clone(),
                            namespace: block.namespace.clone(),
                        })
                        .map_err(|error| error.to_string())?;
                    let arguments = serde_json::to_string(&block.arguments)
                        .map_err(|error| error.to_string())?;
                    for chunk in split_string_by_token_size(
                        &arguments,
                        self.inner.min_token_size,
                        self.inner.max_token_size,
                    ) {
                        schedule_chunk(&chunk, self.inner.tokens_per_second).await;
                        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                            return send_aborted(sender, partial);
                        }
                        sender
                            .send(AssistantMessageEvent::ToolCallDelta {
                                content_index: index,
                                delta: chunk,
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    partial.content[index] = AssistantContent::ToolCall(block.clone());
                    sender
                        .send(AssistantMessageEvent::ToolCallEnd {
                            content_index: index,
                            tool_call: block,
                        })
                        .map_err(|error| error.to_string())?;
                }
            }
        }

        match message.stop_reason {
            StopReason::Pending => Err("Faux response ended without a stop reason".to_owned()),
            StopReason::Error => sender
                .send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Error,
                    error: message,
                })
                .map_err(|error| error.to_string()),
            StopReason::Aborted => sender
                .send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Aborted,
                    error: message,
                })
                .map_err(|error| error.to_string()),
            StopReason::Stop => send_done(sender, SuccessfulStopReason::Stop, message),
            StopReason::Length => send_done(sender, SuccessfulStopReason::Length, message),
            StopReason::ToolUse => send_done(sender, SuccessfulStopReason::ToolUse, message),
            StopReason::Deferred => send_done(sender, SuccessfulStopReason::Deferred, message),
        }
    }

    fn fetch_deferred_response(
        &self,
        request_model: &Model,
        handle: &DeferredHandle,
        fetch_options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        let (sender, stream) = AssistantMessageEventStream::channel();
        self.inner
            .state
            .deferred_fetch_count
            .fetch_add(1, Ordering::SeqCst);
        let core = self.clone();
        let request_model = request_model.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(error) = core
                .run_fetch_deferred(&sender, &request_model, &handle, fetch_options)
                .await
            {
                let message = core.create_error_message(error, &request_model);
                let _ = sender.send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Error,
                    error: message,
                });
            }
        });
        stream
    }

    async fn run_fetch_deferred(
        &self,
        sender: &AssistantStreamSender,
        request_model: &Model,
        handle: &DeferredHandle,
        fetch_options: DeferredFetchOptions,
    ) -> Result<(), String> {
        call_on_response(fetch_options.request.on_response.clone(), request_model).await?;
        let (pending, entry) = {
            let mut entries = self
                .inner
                .deferred_responses
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let entry = entries
                .get_mut(&handle.id)
                .ok_or_else(|| format!("Unknown faux deferred response: {}", handle.id))?;
            if entry.handle.provider != handle.provider
                || entry.handle.model_id != handle.model_id
                || entry.handle.api != handle.api
            {
                return Err(format!("Unknown faux deferred response: {}", handle.id));
            }
            if entry.cancelled {
                return Err(format!(
                    "Faux deferred response was cancelled: {}",
                    handle.id
                ));
            }
            if entry.pending_fetches > 0 {
                entry.pending_fetches -= 1;
                (
                    Some(create_deferred_message(request_model, entry.handle.clone())),
                    None,
                )
            } else {
                (None, Some(entry.clone()))
            }
        };
        if let Some(pending) = pending {
            return self
                .stream_with_deltas(sender, pending, fetch_options.request.signal.clone())
                .await;
        }
        let entry = entry.expect("ready deferred entry");

        let final_message = if let Some(message) = entry.final_message {
            message
        } else {
            let mut submission_options = entry.options;
            if let Some(options) = &mut submission_options {
                options.deferred = None;
                options.stream.request.signal = None;
                options.stream.request.on_response = None;
            }
            let resolved = self
                .resolve_response(
                    entry.step,
                    entry.context,
                    submission_options,
                    entry.model.clone(),
                )
                .await
                .unwrap_or_else(|error| self.create_error_message(error, &entry.model));
            let mut entries = self
                .inner
                .deferred_responses
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(current) = entries.get_mut(&handle.id) {
                current
                    .final_message
                    .get_or_insert_with(|| resolved.clone());
                current.final_message.clone().unwrap_or(resolved)
            } else {
                resolved
            }
        };
        self.stream_with_deltas(sender, final_message, fetch_options.request.signal.clone())
            .await
    }

    fn cancel_deferred_response(
        &self,
        request_model: Model,
        handle: DeferredHandle,
        cancel_options: DeferredCancelOptions,
    ) -> BoxFuture<'static, Result<(), AssistantMessage>> {
        let core = self.clone();
        Box::pin(async move {
            core.inner
                .state
                .cancelled_deferred
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(handle.clone());
            if let Some(entry) = core
                .inner
                .deferred_responses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(&handle.id)
            {
                entry.cancelled = true;
            }
            call_on_response(cancel_options.on_response, &request_model)
                .await
                .map_err(|error| core.create_error_message(error, &request_model))?;
            Ok(())
        })
    }

    fn create_error_message(&self, error: String, model: &Model) -> AssistantMessage {
        let mut message = AssistantMessage::pending(
            self.inner.api.clone(),
            self.inner.provider.clone(),
            model.id.clone(),
            now_ms(),
        );
        message.usage = default_usage();
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error);
        message
    }
}

impl ProviderStreams for FauxCore {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = match options {
            ApiStreamOptions::Base(stream) => stream,
            ApiStreamOptions::OpenAICompletions(options) => options.stream,
            ApiStreamOptions::OpenAIResponses(options) => options.stream,
            ApiStreamOptions::OpenAICodexResponses(options) => options.stream,
            ApiStreamOptions::Custom { base, .. } => base,
        };
        self.stream_response(
            model,
            context,
            Some(SimpleStreamOptions {
                stream,
                ..Default::default()
            }),
        )
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_response(model, context, Some(options))
    }

    fn supports_fetch_deferred(&self) -> bool {
        true
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> Option<AssistantMessageEventStream> {
        Some(self.fetch_deferred_response(model, handle, options))
    }

    fn supports_cancel_deferred(&self) -> bool {
        true
    }

    fn cancel_deferred<'a>(
        &'a self,
        model: &'a Model,
        handle: &'a DeferredHandle,
        options: DeferredCancelOptions,
    ) -> Option<BoxFuture<'a, Result<(), AssistantMessage>>> {
        Some(self.cancel_deferred_response(model.clone(), handle.clone(), options))
    }
}

#[derive(Clone)]
pub struct FauxProviderHandle {
    pub provider: ProviderRef,
    pub api: String,
    pub models: Vec<Model>,
    pub state: Arc<FauxProviderState>,
    core: FauxCore,
}

impl FauxProviderHandle {
    pub fn get_model(&self, model_id: Option<&str>) -> Option<Model> {
        self.core.get_model(model_id)
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.set_responses(responses);
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.append_responses(responses);
    }

    pub fn pending_response_count(&self) -> usize {
        self.core.pending_response_count()
    }
}

pub fn create_faux_core(options: RegisterFauxProviderOptions) -> FauxCore {
    let api = options.api.unwrap_or_else(|| random_id(DEFAULT_API));
    let provider = options
        .provider
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
    let token_size = options.token_size.unwrap_or_default();
    let requested_min = token_size.min.unwrap_or(DEFAULT_MIN_TOKEN_SIZE);
    let requested_max = token_size.max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE);
    let min_token_size = requested_min.min(requested_max).max(1);
    let max_token_size = requested_max.max(min_token_size);
    let definitions = options
        .models
        .filter(|models| !models.is_empty())
        .unwrap_or_else(|| {
            vec![FauxModelDefinition {
                id: DEFAULT_MODEL_ID.to_owned(),
                name: Some(DEFAULT_MODEL_NAME.to_owned()),
                reasoning: Some(false),
                input: Some(vec![ModelInput::Text, ModelInput::Image]),
                cost: Some(ModelCostRates::default()),
                context_window: Some(128_000),
                max_tokens: Some(16_384),
            }]
        });
    let models = definitions
        .into_iter()
        .map(|definition| Model {
            name: definition.name.unwrap_or_else(|| definition.id.clone()),
            id: definition.id,
            api: api.clone().into(),
            provider: provider.clone().into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            reasoning: definition.reasoning.unwrap_or(false),
            thinking_level_map: None,
            input: definition
                .input
                .unwrap_or_else(|| vec![ModelInput::Text, ModelInput::Image]),
            cost: ModelCost {
                rates: definition.cost.unwrap_or_default(),
                tiers: None,
            },
            context_window: definition.context_window.unwrap_or(128_000),
            max_tokens: definition.max_tokens.unwrap_or(16_384),
            sampling_params: None,
            headers: None,
            compat: None,
        })
        .collect();
    FauxCore {
        inner: Arc::new(FauxCoreInner {
            api,
            provider,
            models,
            pending_responses: Mutex::new(VecDeque::new()),
            tokens_per_second: options.tokens_per_second,
            min_token_size,
            max_token_size,
            deferred_options: options.deferred.unwrap_or_default(),
            state: Arc::new(FauxProviderState::default()),
            prompt_cache: Mutex::new(HashMap::new()),
            deferred_responses: Mutex::new(HashMap::new()),
        }),
    }
}

pub fn faux_provider(options: RegisterFauxProviderOptions) -> FauxProviderHandle {
    let core = create_faux_core(options);
    let api: Arc<dyn ProviderStreams> = Arc::new(core.clone());
    let provider = create_provider(CreateProviderOptions {
        id: core.provider_id().to_owned(),
        name: None,
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(ApiKeyAuth {
                name: "Faux".to_owned(),
                login: None,
                check: None,
                resolve: Arc::new(|_| {
                    Box::pin(async {
                        Ok(Some(AuthResult {
                            auth: Default::default(),
                            env: None,
                            source: None,
                        }))
                    })
                }),
            }),
            oauth: None,
        },
        models: core.models().to_vec(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(api),
    });
    FauxProviderHandle {
        provider,
        api: core.api().to_owned(),
        models: core.models().to_vec(),
        state: core.state(),
        core,
    }
}

async fn call_on_response(
    callback: Option<crate::types::OnResponse<Model>>,
    model: &Model,
) -> Result<(), String> {
    if let Some(callback) = callback {
        callback(
            ProviderResponse {
                status: 200,
                headers: BTreeMap::new(),
            },
            model,
        )
        .await?;
    }
    Ok(())
}

fn signal_from_simple(
    options: Option<&SimpleStreamOptions>,
) -> Option<Arc<dyn crate::types::AbortSignal>> {
    options.and_then(|options| options.stream.request.signal.clone())
}

fn deferred_enabled(request: &DeferredRequest) -> bool {
    !matches!(request, DeferredRequest::Enabled(false))
}

fn send_done(
    sender: &AssistantStreamSender,
    reason: SuccessfulStopReason,
    message: AssistantMessage,
) -> Result<(), String> {
    sender
        .send(AssistantMessageEvent::Done { reason, message })
        .map_err(|error| error.to_string())
}

fn send_aborted(sender: &AssistantStreamSender, partial: AssistantMessage) -> Result<(), String> {
    sender
        .send(AssistantMessageEvent::Error {
            reason: ErrorStopReason::Aborted,
            error: create_aborted_message(partial),
        })
        .map_err(|error| error.to_string())
}

fn create_aborted_message(mut partial: AssistantMessage) -> AssistantMessage {
    partial.stop_reason = StopReason::Aborted;
    partial.error_message = Some("Request was aborted".to_owned());
    partial.timestamp = now_ms();
    partial
}

fn create_deferred_message(model: &Model, handle: DeferredHandle) -> AssistantMessage {
    let mut message = AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    message.usage = default_usage();
    message.stop_reason = StopReason::Deferred;
    message.deferred = Some(handle);
    message
}

fn clone_message(
    mut message: AssistantMessage,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    message.api = api.into();
    message.provider = provider.into();
    message.model = model_id.to_owned();
    message
}

fn default_usage() -> Usage {
    Usage {
        input: 0.into(),
        output: 0.into(),
        cache_read: 0.into(),
        cache_write: 0.into(),
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0.into(),
        cost: UsageCost::default(),
    }
}

fn estimate_tokens(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(4)
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(user_content_block_to_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_block_to_text(content: &UserContentBlock) -> String {
    match content {
        UserContentBlock::Text(text) => text.text.clone(),
        UserContentBlock::Image(ImageContent {
            mime_type, data, ..
        }) => format!("[image:{mime_type}:{}]", data.encode_utf16().count()),
    }
}

fn assistant_content_to_text(content: &crate::types::AssistantMessageContent) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(text) => text.text.clone(),
            AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
            AssistantContent::ToolCall(call) => format!(
                "{}:{}",
                call.name,
                serde_json::to_string(&call.arguments).unwrap_or_default()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_to_text(message: &ToolResultMessage) -> String {
    std::iter::once(message.tool_name.clone())
        .chain(message.content.iter().map(user_content_block_to_text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_to_text(message: &Message) -> String {
    match message {
        Message::User(message) => user_content_to_text(&message.content),
        Message::Assistant(message) => assistant_content_to_text(&message.content),
        Message::ToolResult(message) => tool_result_to_text(message),
    }
}

fn serialize_context(context: &Context) -> String {
    let mut parts = Vec::new();
    if let Some(system_prompt) = context
        .system_prompt
        .as_ref()
        .filter(|system_prompt| !system_prompt.is_empty())
    {
        parts.push(format!("system:{system_prompt}"));
    }
    for message in &context.messages {
        let role = match message {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        };
        parts.push(format!("{role}:{}", message_to_text(message)));
    }
    if let Some(tools) = context.tools.as_ref().filter(|tools| !tools.is_empty()) {
        parts.push(format!(
            "tools:{}",
            serde_json::to_string(tools).unwrap_or_default()
        ));
    }
    parts.join("\n\n")
}

fn common_prefix_length(left: &str, right: &str) -> usize {
    left.encode_utf16()
        .zip(right.encode_utf16())
        .take_while(|(left, right)| left == right)
        .count()
}

fn split_string_by_token_size(text: &str, min: usize, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let token_size = min + random_usize(max - min + 1);
        let char_size = token_size.saturating_mul(4).max(1);
        let remaining = &text[start..];
        let mut units = 0;
        let mut end = text.len();
        for (offset, character) in remaining.char_indices() {
            let next_units = units + character.len_utf16();
            if next_units > char_size {
                end = if offset == 0 {
                    start + character.len_utf8()
                } else {
                    start + offset
                };
                break;
            }
            units = next_units;
            if units == char_size {
                end = start + offset + character.len_utf8();
                break;
            }
        }
        chunks.push(text[start..end].to_owned());
        start = end;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    match tokens_per_second.filter(|value| *value > 0.0) {
        Some(tokens_per_second) => {
            let delay_ms = estimate_tokens(chunk) as f64 / tokens_per_second * 1_000.0;
            if delay_ms.is_finite() && delay_ms > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(delay_ms / 1_000.0)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        None => tokio::task::yield_now().await,
    }
}

fn random_usize(upper_exclusive: usize) -> usize {
    if upper_exclusive <= 1 {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    (u64::from_ne_bytes(bytes) as usize) % upper_exclusive
}

fn random_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 8];
    let random = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_ne_bytes(bytes)
    } else {
        0
    };
    format!("{prefix}:{}:{}", now_ms(), base36(random))
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut encoded = Vec::new();
    while value > 0 {
        encoded.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    encoded.reverse();
    String::from_utf8(encoded).expect("base36 is ASCII")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{UserMessage, UserRole};
    use crate::utils::abort::{AbortController, AbortReason};
    use futures::StreamExt;

    fn context(text: &str) -> Context {
        Context {
            system_prompt: None,
            messages: vec![Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text(text.to_owned()),
                timestamp: 0,
            }))],
            tools: None,
        }
    }

    fn text(message: &AssistantMessage) -> &str {
        match &message.content[0] {
            AssistantContent::Text(content) => &content.text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    async fn complete(
        core: &FauxCore,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessage {
        core.stream_simple(model, context, options)
            .result()
            .await
            .expect("terminal message")
    }

    async fn events(
        core: &FauxCore,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> Vec<AssistantMessageEvent> {
        let mut stream = core.stream_simple(model, context, options);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    fn event_name(event: &AssistantMessageEvent) -> &'static str {
        match event {
            AssistantMessageEvent::Start => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        }
    }

    /// Pins pi `src/providers/faux.ts:208-210`.
    #[test]
    fn empty_system_prompt_is_omitted_from_serialized_context() {
        let without_system_prompt = context("hi");
        let with_empty_system_prompt = Context {
            system_prompt: Some(String::new()),
            ..context("hi")
        };

        assert_eq!(serialize_context(&with_empty_system_prompt), "user:hi");
        assert_eq!(
            serialize_context(&with_empty_system_prompt),
            serialize_context(&without_system_prompt)
        );
    }

    /// Pins pi `src/providers/faux.ts:646-650`.
    #[test]
    fn empty_model_id_selects_the_default_model() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());

        assert_eq!(
            core.get_model(Some("")).map(|model| model.id),
            core.get_model(None).map(|model| model.id)
        );
    }

    /// Pins pi `src/providers/faux.ts:269-278`; Rust strings keep surrogate pairs intact.
    #[test]
    fn token_sized_chunks_count_utf16_units_without_losing_scalars() {
        let chunks = split_string_by_token_size("abc😀defg", 1, 1);
        assert_eq!(chunks, ["abc", "😀de", "fg"]);
        assert_eq!(chunks.concat(), "abc😀defg");
        assert!(chunks.iter().all(|chunk| chunk.encode_utf16().count() <= 4));
    }

    /// Ports pi `test/faux-provider.test.ts:31-68`.
    #[tokio::test]
    async fn helpers_stream_content_and_estimate_usage() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message(
                vec![
                    faux_thinking("think"),
                    faux_tool_call("echo", serde_json::json!({"text": "hi"}), None),
                    faux_text("done"),
                ],
                FauxAssistantMessageOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let response = complete(
            &core,
            &model,
            &Context {
                system_prompt: Some("Be concise.".to_owned()),
                ..context("hi there")
            },
            SimpleStreamOptions::default(),
        )
        .await;
        assert_eq!(response.content.len(), 3);
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(response.usage.input.as_number() > 0.0);
        assert!(response.usage.output.as_number() > 0.0);
        assert_eq!(
            response.usage.total_tokens.as_number(),
            response.usage.input.as_number() + response.usage.output.as_number()
        );
        assert_eq!(core.state().call_count(), 1);
    }

    /// Ports pi `test/faux-provider.test.ts:70-115`.
    #[tokio::test]
    async fn multiple_models_are_model_aware_and_response_identity_is_rewritten() {
        let core = create_faux_core(RegisterFauxProviderOptions {
            api: Some("faux:test".to_owned()),
            provider: Some("faux-provider".to_owned()),
            models: Some(vec![
                FauxModelDefinition {
                    reasoning: Some(false),
                    ..FauxModelDefinition::new("faux-fast")
                },
                FauxModelDefinition {
                    name: Some("Faux Thinker".to_owned()),
                    reasoning: Some(true),
                    ..FauxModelDefinition::new("faux-thinker")
                },
            ]),
            ..Default::default()
        });
        let factory: FauxResponseFactory = Arc::new(|_, _, _, model| {
            Box::pin(async move {
                Ok(faux_assistant_message(
                    format!("{}:{}", model.id, model.reasoning),
                    Default::default(),
                ))
            })
        });
        core.set_responses(vec![
            FauxResponseStep::Factory(factory.clone()),
            FauxResponseStep::Factory(factory),
        ]);
        let fast_model = core.get_model(Some("faux-fast")).expect("fast");
        let thinker_model = core.get_model(Some("faux-thinker")).expect("thinker");
        assert!(!fast_model.reasoning);
        assert!(thinker_model.reasoning);
        let fast = complete(&core, &fast_model, &context("hi"), Default::default()).await;
        let thinker = complete(&core, &thinker_model, &context("hi"), Default::default()).await;
        assert_eq!(text(&fast), "faux-fast:false");
        assert_eq!(text(&thinker), "faux-thinker:true");
        assert_eq!(thinker.api.as_str(), "faux:test");
        assert_eq!(thinker.provider.as_str(), "faux-provider");
        assert_eq!(thinker.model, "faux-thinker");
    }

    /// Ports pi `test/faux-provider.test.ts:117-194`.
    #[tokio::test]
    async fn queues_replace_append_and_factory_failures_are_in_band() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message("first", Default::default()).into(),
            faux_assistant_message("second", Default::default()).into(),
        ]);
        assert_eq!(core.pending_response_count(), 2);
        assert_eq!(
            text(&complete(&core, &model, &context("hi"), Default::default()).await),
            "first"
        );
        assert_eq!(
            text(&complete(&core, &model, &context("hi"), Default::default()).await),
            "second"
        );
        let exhausted = complete(&core, &model, &context("hi"), Default::default()).await;
        assert_eq!(exhausted.stop_reason, StopReason::Error);
        assert_eq!(
            exhausted.error_message.as_deref(),
            Some("No more faux responses queued")
        );

        core.set_responses(vec![
            faux_assistant_message("replacement", Default::default()).into(),
        ]);
        core.append_responses(vec![
            faux_assistant_message("appended", Default::default()).into(),
        ]);
        assert_eq!(core.pending_response_count(), 2);
        assert_eq!(
            text(&complete(&core, &model, &context("hi"), Default::default()).await),
            "replacement"
        );
        assert_eq!(
            text(&complete(&core, &model, &context("hi"), Default::default()).await),
            "appended"
        );

        core.set_responses(vec![FauxResponseStep::Factory(Arc::new(|_, _, _, _| {
            Box::pin(async { Err("boom".to_owned()) })
        }))]);
        let failure = events(&core, &model, &context("hi"), Default::default()).await;
        assert_eq!(failure.len(), 1);
        match &failure[0] {
            AssistantMessageEvent::Error { error, .. } => {
                assert_eq!(error.error_message.as_deref(), Some("boom"));
            }
            event => panic!("expected error, got {event:?}"),
        }
    }

    /// Ports pi `test/faux-provider.test.ts:196-264`.
    #[tokio::test]
    async fn pending_response_is_rejected_and_context_usage_uses_utf16_lengths() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message(
                "partial",
                FauxAssistantMessageOptions {
                    stop_reason: Some(StopReason::Pending),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let invalid = events(&core, &model, &context("hi"), Default::default()).await;
        assert!(
            !invalid
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Done { .. }))
        );
        match invalid.last().expect("terminal") {
            AssistantMessageEvent::Error { error, .. } => assert_eq!(
                error.error_message.as_deref(),
                Some("Faux response ended without a stop reason")
            ),
            event => panic!("expected error, got {event:?}"),
        }

        core.set_responses(vec![
            faux_assistant_message("done", Default::default()).into(),
        ]);
        let usage = complete(&core, &model, &context("hello 😀"), Default::default()).await;
        let prompt = "user:hello 😀";
        assert_eq!(
            usage.usage.input.as_number(),
            prompt.encode_utf16().count().div_ceil(4) as f64
        );
        assert_eq!(usage.usage.output.as_number(), 1.0);
    }

    /// Ports pi `test/faux-provider.test.ts:266-345`.
    #[tokio::test]
    async fn prompt_cache_is_scoped_by_session_and_disabled_by_none() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        let model = core.get_model(None).expect("model");
        core.set_responses(
            ["first", "second", "third", "fourth"]
                .into_iter()
                .map(|text| faux_assistant_message(text, Default::default()).into())
                .collect(),
        );
        let cached_options = |session: &str, retention| SimpleStreamOptions {
            stream: crate::types::StreamOptions {
                session_id: Some(session.to_owned()),
                cache_retention: Some(retention),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ctx = context("hello");
        let first = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-1", CacheRetention::Short),
        )
        .await;
        assert_eq!(first.usage.cache_read.as_number(), 0.0);
        assert!(first.usage.cache_write.as_number() > 0.0);
        ctx.messages.push(Message::Assistant(Box::new(first)));
        ctx.messages.push(Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("follow up".to_owned()),
            timestamp: 1,
        })));
        let second = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-1", CacheRetention::Short),
        )
        .await;
        assert!(second.usage.cache_read.as_number() > 0.0);
        let separate = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-2", CacheRetention::Short),
        )
        .await;
        assert_eq!(separate.usage.cache_read.as_number(), 0.0);
        let none = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-1", CacheRetention::None),
        )
        .await;
        assert_eq!(none.usage.cache_read.as_number(), 0.0);
        assert_eq!(none.usage.cache_write.as_number(), 0.0);
    }

    /// Ports pi `test/faux-provider.test.ts:347-430`.
    #[tokio::test]
    async fn fixed_chunks_stream_exact_content_event_order() {
        let core = create_faux_core(RegisterFauxProviderOptions {
            token_size: Some(FauxTokenSizeOptions {
                min: Some(1),
                max: Some(1),
            }),
            ..Default::default()
        });
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message(
                vec![
                    faux_thinking("go"),
                    faux_text("ok"),
                    faux_tool_call("echo", serde_json::json!({}), Some("tool-1".to_owned())),
                ],
                FauxAssistantMessageOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
            .into(),
        ]);
        let events = events(&core, &model, &context("hi"), Default::default()).await;
        assert_eq!(
            events.iter().map(event_name).collect::<Vec<_>>(),
            [
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_end",
                "done",
            ]
        );
        let tool_json = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<Value>(&tool_json).expect("arguments"),
            serde_json::json!({})
        );
    }

    /// Ports pi `test/faux-provider.test.ts:432-503`.
    #[tokio::test]
    async fn explicit_errors_and_preaborted_requests_are_terminal_errors() {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        let model = core.get_model(None).expect("model");
        for (stop_reason, expected_reason) in [
            (StopReason::Error, ErrorStopReason::Error),
            (StopReason::Aborted, ErrorStopReason::Aborted),
        ] {
            core.set_responses(vec![
                faux_assistant_message(
                    "partial",
                    FauxAssistantMessageOptions {
                        stop_reason: Some(stop_reason),
                        error_message: Some("terminal".to_owned()),
                        ..Default::default()
                    },
                )
                .into(),
            ]);
            let events = events(&core, &model, &context("hi"), Default::default()).await;
            match events.last().expect("terminal") {
                AssistantMessageEvent::Error { reason, error } => {
                    assert_eq!(*reason, expected_reason);
                    assert_eq!(error.stop_reason, stop_reason);
                }
                event => panic!("expected error, got {event:?}"),
            }
        }

        core.set_responses(vec![
            faux_assistant_message("ignored", Default::default()).into(),
        ]);
        let controller = AbortController::new();
        controller.abort(AbortReason::default_abort());
        let options = SimpleStreamOptions {
            stream: crate::types::StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    signal: Some(controller.signal()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let events = events(&core, &model, &context("hi"), options).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            AssistantMessageEvent::Error {
                reason: ErrorStopReason::Aborted,
                ..
            }
        ));
    }

    /// Ports pi `test/faux-provider.test.ts:505-605` for each streamed block kind.
    #[tokio::test]
    async fn paced_stream_can_abort_after_the_first_delta() {
        for content in [
            faux_text("abcdefghijklmnopqrstuvwxyz"),
            faux_thinking("abcdefghijklmnopqrstuvwxyz"),
            faux_tool_call(
                "echo",
                serde_json::json!({"text": "abcdefghijklmnopqrstuvwxyz", "count": 123456789}),
                Some("tool-1".to_owned()),
            ),
        ] {
            let core = create_faux_core(RegisterFauxProviderOptions {
                tokens_per_second: Some(100.0),
                token_size: Some(FauxTokenSizeOptions {
                    min: Some(3),
                    max: Some(3),
                }),
                ..Default::default()
            });
            let model = core.get_model(None).expect("model");
            core.set_responses(vec![
                faux_assistant_message(
                    content,
                    FauxAssistantMessageOptions {
                        stop_reason: Some(StopReason::ToolUse),
                        ..Default::default()
                    },
                )
                .into(),
            ]);
            let controller = AbortController::new();
            let options = SimpleStreamOptions {
                stream: crate::types::StreamOptions {
                    request: crate::types::ProviderRequestOptions {
                        signal: Some(controller.signal()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut stream = core.stream_simple(&model, &context("hi"), options);
            let mut delta_count = 0;
            let mut saw_error = false;
            while let Some(event) = stream.next().await {
                if matches!(
                    event,
                    AssistantMessageEvent::TextDelta { .. }
                        | AssistantMessageEvent::ThinkingDelta { .. }
                        | AssistantMessageEvent::ToolCallDelta { .. }
                ) {
                    delta_count += 1;
                    controller.abort(AbortReason::default_abort());
                }
                saw_error |= matches!(event, AssistantMessageEvent::Error { .. });
            }
            assert_eq!(delta_count, 1);
            assert!(saw_error);
        }
    }

    /// Pins pi `src/providers/faux.ts:524-642` deferred submission, polling, replay, and cancellation.
    #[tokio::test]
    async fn deferred_responses_poll_replay_and_cancel() {
        let core = create_faux_core(RegisterFauxProviderOptions {
            deferred: Some(FauxDeferredOptions {
                pending_fetches: Some(1),
                poll_after_ms: Some(25),
            }),
            ..Default::default()
        });
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message("ready", Default::default()).into(),
        ]);
        let submit_options = SimpleStreamOptions {
            deferred: Some(DeferredRequest::Enabled(true)),
            ..Default::default()
        };
        let submitted = complete(&core, &model, &context("hi"), submit_options).await;
        assert_eq!(submitted.stop_reason, StopReason::Deferred);
        let handle = submitted.deferred.expect("deferred handle");
        assert_eq!(handle.poll_after_ms, Some(25));

        let first = core
            .fetch_deferred(&model, &handle, DeferredFetchOptions::default())
            .expect("fetch")
            .result()
            .await
            .expect("pending poll");
        assert_eq!(first.stop_reason, StopReason::Deferred);
        let ready = core
            .fetch_deferred(&model, &handle, DeferredFetchOptions::default())
            .expect("fetch")
            .result()
            .await
            .expect("ready poll");
        assert_eq!(text(&ready), "ready");
        let replay = core
            .fetch_deferred(&model, &handle, DeferredFetchOptions::default())
            .expect("fetch")
            .result()
            .await
            .expect("replay");
        assert_eq!(text(&replay), "ready");
        assert_eq!(core.state().deferred_fetch_count(), 3);

        core.cancel_deferred(&model, &handle, DeferredCancelOptions::default())
            .expect("cancel")
            .await
            .expect("cancel response");
        assert_eq!(
            core.state().cancelled_deferred(),
            std::slice::from_ref(&handle)
        );
        let cancelled = core
            .fetch_deferred(&model, &handle, DeferredFetchOptions::default())
            .expect("fetch")
            .result()
            .await
            .expect("cancelled terminal");
        assert_eq!(cancelled.stop_reason, StopReason::Error);
        assert!(
            cancelled
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("was cancelled"))
        );
    }
}
