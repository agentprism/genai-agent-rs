use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::auth::types::{ApiKeyAuth, AuthResult, ProviderAuth};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, DeferredCancelOptions,
    DeferredFetchOptions, DeferredHandle, DeferredRequest, ErrorStopReason, ImageContent, JsString,
    JsonObject, Message, Model, ModelCost, ModelCostRates, ModelInput, ProviderResponse,
    SimpleStreamOptions, StopReason, SuccessfulStopReason, TextContent, ThinkingContent, ToolCall,
    ToolResultMessage, Usage, UsageCost, UserContent, UserContentBlock,
};
use crate::utils::ecma_json::stringify_object;
use futures::future::BoxFuture;
#[cfg(test)]
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
const DEFAULT_MIN_TOKEN_SIZE: f64 = 3.0;
const DEFAULT_MAX_TOKEN_SIZE: f64 = 5.0;

#[derive(Debug, Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<ModelInput>>,
    pub cost: Option<ModelCostRates>,
    pub context_window: Option<f64>,
    pub max_tokens: Option<f64>,
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

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum FauxAssistantContent {
    Text(JsString),
    Block(AssistantContent),
    Blocks(Vec<AssistantContent>),
}

impl From<&str> for FauxAssistantContent {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}

impl From<String> for FauxAssistantContent {
    fn from(value: String) -> Self {
        Self::Text(value.into())
    }
}

impl From<JsString> for FauxAssistantContent {
    fn from(value: JsString) -> Self {
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
    pub error_message: Option<JsString>,
    pub response_id: Option<JsString>,
    pub timestamp: Option<f64>,
}

pub fn faux_text(text: impl Into<JsString>) -> AssistantContent {
    AssistantContent::Text(TextContent::new(text))
}

pub fn faux_thinking(thinking: impl Into<JsString>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent::new(thinking))
}

pub fn faux_tool_call(
    name: impl Into<JsString>,
    arguments: impl Into<JsonObject>,
    id: Option<JsString>,
) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::new(
        id.unwrap_or_else(|| random_id("tool").into()),
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
    message.content = content;
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
    pub pending_fetches: Option<f64>,
    pub poll_after_ms: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxTokenSizeOptions {
    pub min: Option<f64>,
    pub max: Option<f64>,
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
    pending_fetches: f64,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

struct FauxCoreInner {
    api: String,
    provider: String,
    models: Vec<Model>,
    pending_responses: Mutex<VecDeque<FauxResponseStep>>,
    tokens_per_second: Option<f64>,
    min_token_size: f64,
    max_token_size: f64,
    deferred_options: FauxDeferredOptions,
    state: Arc<FauxProviderState>,
    prompt_cache: Mutex<HashMap<String, JsString>>,
    deferred_responses: Mutex<HashMap<JsString, DeferredEntry>>,
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
                model_id: request_model.id.clone().into(),
                api: request_model.api.0.clone(),
                id: random_id("deferred").into(),
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
                        pending_fetches: normalize_pending_fetches(
                            self.inner.deferred_options.pending_fetches,
                        ),
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
        let mut cache_read = 0.0;
        let mut cache_write = 0.0;

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
                cache_read = (cached_units as f64 / 4.0).ceil();
                cache_write =
                    ((prompt_text.len().saturating_sub(cached_units)) as f64 / 4.0).ceil();
                input = (prompt_tokens - cache_read).max(0.0);
            } else {
                cache_write = prompt_tokens;
            }
            prompt_cache.insert(session_id.to_owned(), prompt_text);
        }

        message.usage = Usage {
            input,
            output: output_tokens,
            cache_read,
            cache_write,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output_tokens + cache_read + cache_write,
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
        partial.content = Vec::new();
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
            .send(AssistantMessageEvent::Start {
                partial: Arc::new(partial.clone()),
            })
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
                            content_index: index as f64,
                            partial: Arc::new(partial.clone()),
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
                                content_index: index as f64,
                                delta: chunk,
                                partial: Arc::new(partial.clone()),
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    sender
                        .send(AssistantMessageEvent::ThinkingEnd {
                            content_index: index as f64,
                            content: block.thinking,
                            partial: Arc::new(partial.clone()),
                        })
                        .map_err(|error| error.to_string())?;
                }
                AssistantContent::Text(block) => {
                    partial
                        .content
                        .push(AssistantContent::Text(TextContent::new("")));
                    sender
                        .send(AssistantMessageEvent::TextStart {
                            content_index: index as f64,
                            partial: Arc::new(partial.clone()),
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
                                content_index: index as f64,
                                delta: chunk,
                                partial: Arc::new(partial.clone()),
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    sender
                        .send(AssistantMessageEvent::TextEnd {
                            content_index: index as f64,
                            content: block.text,
                            partial: Arc::new(partial.clone()),
                        })
                        .map_err(|error| error.to_string())?;
                }
                AssistantContent::ToolCall(block) => {
                    partial
                        .content
                        .push(AssistantContent::ToolCall(ToolCall::new(
                            block.id.clone(),
                            block.name.clone(),
                            serde_json::Map::new(),
                        )));
                    sender
                        .send(AssistantMessageEvent::ToolCallStart {
                            content_index: index as f64,
                            partial: Arc::new(partial.clone()),
                        })
                        .map_err(|error| error.to_string())?;
                    let arguments = JsString::from(stringify_object(&block.arguments));
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
                                content_index: index as f64,
                                delta: chunk,
                                partial: Arc::new(partial.clone()),
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    partial.content[index] = AssistantContent::ToolCall(block.clone());
                    sender
                        .send(AssistantMessageEvent::ToolCallEnd {
                            content_index: index as f64,
                            tool_call: block,
                            partial: Arc::new(partial.clone()),
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
            if entry.pending_fetches > 0.0 {
                entry.pending_fetches -= 1.0;
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
        message.error_message = Some(error.into());
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
            ApiStreamOptions::AnthropicMessages(options) => options.stream,
            ApiStreamOptions::BedrockConverseStream(options) => options.stream,
            ApiStreamOptions::OpenAICompletions(options) => options.stream,
            ApiStreamOptions::OpenAIResponses(options) => options.stream,
            ApiStreamOptions::OpenAICodexResponses(options) => options.stream,
            ApiStreamOptions::GoogleGenerativeAI(options) => options.stream,
            ApiStreamOptions::GoogleVertex(options) => options.stream,
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
    let min_token_size = js_math_max(1.0, js_math_min(requested_min, requested_max));
    let max_token_size = js_math_max(min_token_size, requested_max);
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
                context_window: Some(128_000.0),
                max_tokens: Some(16_384.0),
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
            context_window: definition.context_window.unwrap_or(128_000.0),
            max_tokens: definition.max_tokens.unwrap_or(16_384.0),
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
                status: 200.0,
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
    partial.error_message = Some("Request was aborted".into());
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
    message.model = model_id.into();
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

fn estimate_tokens(text: &JsString) -> f64 {
    (text.len() as f64 / 4.0).ceil()
}

fn user_content_to_text(content: &UserContent) -> JsString {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => join_js_strings(
            blocks.iter().map(user_content_block_to_text),
            &JsString::from("\n"),
        ),
    }
}

fn user_content_block_to_text(content: &UserContentBlock) -> JsString {
    match content {
        UserContentBlock::Text(text) => text.text.clone(),
        UserContentBlock::Image(ImageContent {
            mime_type, data, ..
        }) => format!("[image:{mime_type}:{}]", data.encode_utf16().count()).into(),
    }
}

fn assistant_content_to_text(content: &crate::types::AssistantMessageContent) -> JsString {
    join_js_strings(
        content.iter().map(|block| match block {
            AssistantContent::Text(text) => text.text.clone(),
            AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
            AssistantContent::ToolCall(call) => {
                format!("{}:{}", call.name, stringify_object(&call.arguments)).into()
            }
        }),
        &JsString::from("\n"),
    )
}

fn tool_result_to_text(message: &ToolResultMessage) -> JsString {
    join_js_strings(
        std::iter::once(message.tool_name.clone())
            .chain(message.content.iter().map(user_content_block_to_text)),
        &JsString::from("\n"),
    )
}

fn message_to_text(message: &Message) -> JsString {
    match message {
        Message::User(message) => user_content_to_text(&message.content),
        Message::Assistant(message) => assistant_content_to_text(&message.content),
        Message::ToolResult(message) => tool_result_to_text(message),
    }
}

fn serialize_context(context: &Context) -> JsString {
    let mut parts = Vec::new();
    if let Some(system_prompt) = context
        .system_prompt
        .as_ref()
        .filter(|system_prompt| !system_prompt.is_empty())
    {
        let mut part = JsString::from("system:");
        part.push_str(system_prompt);
        parts.push(part);
    }
    for message in &context.messages {
        let role = match message {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        };
        let mut part = JsString::from(format!("{role}:"));
        part.push_str(message_to_text(message));
        parts.push(part);
    }
    if let Some(tools) = context.tools.as_ref().filter(|tools| !tools.is_empty()) {
        parts.push(format!("tools:{}", serde_json::to_string(tools).unwrap_or_default()).into());
    }
    join_js_strings(parts, &JsString::from("\n\n"))
}

fn common_prefix_length(left: &JsString, right: &JsString) -> usize {
    left.as_utf16()
        .iter()
        .zip(right.as_utf16())
        .take_while(|(left, right)| left == right)
        .count()
}

fn split_string_by_token_size(text: &JsString, min: f64, max: f64) -> Vec<JsString> {
    let mut chunks = Vec::new();
    let mut start = 0.0;
    while start < text.len() as f64 {
        let token_size = min + (random_unit() * (max - min + 1.0)).floor();
        let char_size = js_math_max(1.0, token_size * 4.0);
        let end = start + char_size;
        chunks.push(text.slice(
            ecma_slice_index(start, text.len()),
            ecma_slice_index(end, text.len()),
        ));
        start = end;
    }
    if chunks.is_empty() {
        chunks.push(JsString::default());
    }
    chunks
}

async fn schedule_chunk(chunk: &JsString, tokens_per_second: Option<f64>) {
    match tokens_per_second.filter(|value| *value > 0.0) {
        Some(tokens_per_second) => {
            let delay_ms = estimate_tokens(chunk) / tokens_per_second * 1_000.0;
            if delay_ms.is_finite() && delay_ms > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(delay_ms / 1_000.0)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        None => tokio::task::yield_now().await,
    }
}

fn join_js_strings(values: impl IntoIterator<Item = JsString>, separator: &JsString) -> JsString {
    let mut output = JsString::default();
    for (index, value) in values.into_iter().enumerate() {
        if index != 0 {
            output.push_str(separator);
        }
        output.push_str(&value);
    }
    output
}

fn random_unit() -> f64 {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.0;
    }
    ((u64::from_ne_bytes(bytes) >> 11) as f64) / ((1_u64 << 53) as f64)
}

fn js_math_min(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else {
        left.min(right)
    }
}

fn js_math_max(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else {
        left.max(right)
    }
}

fn normalize_pending_fetches(value: Option<f64>) -> f64 {
    js_math_max(0.0, value.unwrap_or_default().floor())
}

fn ecma_slice_index(value: f64, len: usize) -> usize {
    if value.is_nan() || value == f64::NEG_INFINITY {
        0
    } else if value == f64::INFINITY {
        len
    } else {
        value.trunc().clamp(0.0, len as f64) as usize
    }
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

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX) as f64
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
                content: UserContent::Text((text.to_owned()).into()),
                timestamp: 0.0,
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
            AssistantMessageEvent::Start { .. } => "start",
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
            system_prompt: Some((String::new()).into()),
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

    /// Pins pi `src/providers/faux.ts:269-278` exact UTF-16 `slice` boundaries.
    #[test]
    fn token_sized_chunks_split_astral_pairs_exactly_like_pi() {
        for (source, expected) in [
            (
                "ab😀X",
                vec![&[0x61, 0x62, 0xd83d, 0xde00][..], &[0x58][..]],
            ),
            (
                "abc😀defg",
                vec![
                    &[0x61, 0x62, 0x63, 0xd83d][..],
                    &[0xde00, 0x64, 0x65, 0x66][..],
                    &[0x67][..],
                ],
            ),
            (
                "abcd😀X",
                vec![&[0x61, 0x62, 0x63, 0x64][..], &[0xd83d, 0xde00, 0x58][..]],
            ),
        ] {
            let source = JsString::from(source);
            let chunks = split_string_by_token_size(&source, 1.0, 1.0);
            assert_eq!(
                chunks
                    .iter()
                    .map(|chunk| chunk.as_utf16())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(join_js_strings(chunks, &JsString::from("")), source);
        }
        let split = split_string_by_token_size(&JsString::from("abc😀defg"), 1.0, 1.0);
        assert_eq!(
            serde_json::to_string(&split[0]).expect("high-surrogate JSON"),
            r#""abc\ud83d""#
        );
        assert_eq!(
            serde_json::to_string(&split[1]).expect("low-surrogate JSON"),
            r#""\ude00def""#
        );
    }

    /// Pins pi `src/providers/faux.ts:269-278,436-443,538`: public numeric
    /// options retain JavaScript-number inputs and coerce only when consumed.
    #[test]
    fn fractional_nonfinite_and_signed_faux_numbers_follow_javascript_coercion() {
        let source = JsString::from("abcdefghijkl");
        assert_eq!(
            split_string_by_token_size(&source, 1.5, 1.5)
                .into_iter()
                .map(|chunk| chunk.to_utf8().expect("ASCII chunk"))
                .collect::<Vec<_>>(),
            vec!["abcdef".to_owned(), "ghijkl".to_owned()]
        );
        assert_eq!(
            split_string_by_token_size(&source, f64::INFINITY, f64::INFINITY),
            vec![JsString::default()]
        );
        assert_eq!(
            split_string_by_token_size(&source, f64::NAN, f64::NAN),
            vec![JsString::default()]
        );

        assert_eq!(normalize_pending_fetches(Some(-1.0)), 0.0);
        assert_eq!(normalize_pending_fetches(Some(1.9)), 1.0);
        assert!(normalize_pending_fetches(Some(f64::NAN)).is_nan());
        assert_eq!(
            normalize_pending_fetches(Some(f64::INFINITY)),
            f64::INFINITY
        );

        let core = create_faux_core(RegisterFauxProviderOptions {
            token_size: Some(FauxTokenSizeOptions {
                min: Some(-2.0),
                max: Some(-1.0),
            }),
            deferred: Some(FauxDeferredOptions {
                pending_fetches: Some(f64::NAN),
                poll_after_ms: None,
            }),
            ..Default::default()
        });
        assert_eq!(core.inner.min_token_size, 1.0);
        assert_eq!(core.inner.max_token_size, 1.0);
        assert!(
            core.inner
                .deferred_options
                .pending_fetches
                .unwrap()
                .is_nan()
        );
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
                    faux_tool_call(
                        "echo",
                        JsonObject::try_from(serde_json::json!({"text": "hi"}))
                            .expect("object arguments"),
                        None,
                    ),
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
                system_prompt: Some(("Be concise.".to_owned()).into()),
                ..context("hi there")
            },
            SimpleStreamOptions::default(),
        )
        .await;
        assert_eq!(response.content.len(), 3);
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(response.usage.input > 0.0);
        assert!(response.usage.output > 0.0);
        assert_eq!(
            response.usage.total_tokens,
            response.usage.input + response.usage.output
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
            usage.usage.input,
            prompt.encode_utf16().count().div_ceil(4) as f64
        );
        assert_eq!(usage.usage.output, 1.0);
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
        assert_eq!(first.usage.cache_read, 0.0);
        assert!(first.usage.cache_write > 0.0);
        ctx.messages.push(Message::Assistant(Box::new(first)));
        ctx.messages.push(Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(("follow up".to_owned()).into()),
            timestamp: 1.0,
        })));
        let second = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-1", CacheRetention::Short),
        )
        .await;
        assert!(second.usage.cache_read > 0.0);
        let separate = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-2", CacheRetention::Short),
        )
        .await;
        assert_eq!(separate.usage.cache_read, 0.0);
        let none = complete(
            &core,
            &model,
            &ctx,
            cached_options("session-1", CacheRetention::None),
        )
        .await;
        assert_eq!(none.usage.cache_read, 0.0);
        assert_eq!(none.usage.cache_write, 0.0);
    }

    /// Ports pi `test/faux-provider.test.ts:347-430`.
    #[tokio::test]
    async fn fixed_chunks_stream_exact_content_event_order() {
        let core = create_faux_core(RegisterFauxProviderOptions {
            token_size: Some(FauxTokenSizeOptions {
                min: Some(1.0),
                max: Some(1.0),
            }),
            ..Default::default()
        });
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message(
                vec![
                    faux_thinking("go"),
                    faux_text("ok"),
                    faux_tool_call("echo", JsonObject::new(), Some("tool-1".into())),
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

    /// Pins pi `types.ts:536-546` and `src/providers/faux.ts:327-433`:
    /// each emitted event exposes the exact snapshot after that transition.
    #[tokio::test]
    async fn emitted_partial_snapshots_track_faux_text_deltas() {
        let core = create_faux_core(RegisterFauxProviderOptions {
            token_size: Some(FauxTokenSizeOptions {
                min: Some(1.0),
                max: Some(1.0),
            }),
            ..Default::default()
        });
        let model = core.get_model(None).expect("model");
        core.set_responses(vec![
            faux_assistant_message(
                vec![faux_text("abc😀defg")],
                FauxAssistantMessageOptions::default(),
            )
            .into(),
        ]);
        let events = events(&core, &model, &context("hi"), Default::default()).await;
        let mut accumulated = JsString::new();
        for event in &events {
            if let AssistantMessageEvent::TextDelta { delta, partial, .. } = event {
                accumulated.push_str(delta);
                let AssistantContent::Text(text) = &partial.content[0] else {
                    panic!("text snapshot")
                };
                assert_eq!(text.text, accumulated);
            }
            if !matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                assert!(event.partial().is_some());
            }
        }
        assert_eq!(accumulated, "abc😀defg");
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
                        error_message: Some("terminal".into()),
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
                JsonObject::try_from(
                    serde_json::json!({"text": "abcdefghijklmnopqrstuvwxyz", "count": 123456789}),
                )
                .expect("object arguments"),
                Some("tool-1".into()),
            ),
        ] {
            let core = create_faux_core(RegisterFauxProviderOptions {
                tokens_per_second: Some(100.0),
                token_size: Some(FauxTokenSizeOptions {
                    min: Some(3.0),
                    max: Some(3.0),
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
                pending_fetches: Some(1.0),
                poll_after_ms: Some(25.0),
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
        assert_eq!(handle.poll_after_ms, Some(25.0));

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
