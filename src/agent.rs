//! Cloneable stateful facade over the low-level agent loop.
//!
//! [`Agent`] owns transcript state, runtime configuration, steering and follow-up queues, listener
//! registrations, and one optional active run. A run consumes owned state/configuration snapshots;
//! guarded runtime-configuration setters serialize with run admission and return
//! [`AgentError::Busy`] while a run is active. Observable events update [`AgentState`] before
//! listeners are called, and listeners are awaited in registration order as part of run settlement.
//!
//! Cancellation is cooperative through the active run's [`CancellationToken`]. Queue operations and
//! state snapshots remain available through any clone because all clones share the same agent.

use crate::{
    AfterToolCallHook, AgentContext, AgentError, AgentEvent, AgentLoopConfig, AgentMessage,
    AgentPrepareNextTurnHook, AgentPrepareNextTurnWithContextHook, AgentShouldStopAfterTurnHook,
    AgentTool, AssistantContent, AssistantMessage, BeforeToolCallHook, BusyContext, ConvertToLlm,
    QueueMode, StopReason, StreamFn, ThinkingLevel, ToolExecutionMode, TransformContextHook,
    UserContent, UserMessage, default_convert_to_llm, get_default_stream_fn, run_agent_loop,
    run_agent_loop_continue,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use genai::adapter::AdapterKind;
use genai::chat::ChatOptions;
use genai::{ModelIden, ModelSpec};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use tokio_util::sync::CancellationToken;

/// Point-in-time snapshot of all publicly observable agent state.
///
/// [`Agent::state`] returns this value by cloning the stored transcript and tool handles. During a
/// run, each lifecycle event is applied to state before registered listeners receive that event.
#[derive(Clone)]
pub struct AgentState {
    /// System instruction captured for the next low-level run snapshot.
    pub system_prompt: String,
    /// Model captured for the next low-level run snapshot.
    pub model: ModelSpec,
    /// Reasoning request captured for the next low-level run snapshot.
    pub thinking_level: ThinkingLevel,
    /// Tools captured for the next low-level run snapshot.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Committed conversation transcript.
    ///
    /// A message is appended when its [`AgentEvent::MessageEnd`] event is processed.
    pub messages: Vec<AgentMessage>,
    /// Whether a prompt or continuation has been admitted and has not fully settled.
    ///
    /// This remains `true` while event listeners are being awaited.
    pub is_streaming: bool,
    /// Latest partial message between its start/update and end events.
    pub streaming_message: Option<AgentMessage>,
    /// Tool-call identifiers whose start event has no matching end event yet.
    pub pending_tool_calls: HashSet<String>,
    /// Error text from the most recently admitted run's latest failed assistant turn, if any.
    ///
    /// Admission clears the previous value; successful turns do not populate it.
    pub error_message: Option<String>,
}

impl std::fmt::Debug for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentState")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.spec())
                    .collect::<Vec<_>>(),
            )
            .field("messages", &self.messages)
            .field("is_streaming", &self.is_streaming)
            .field("streaming_message", &self.streaming_message)
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish()
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: ModelSpec::from_iden(crate::assistant::unknown_model_iden()),
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        }
    }
}

/// Awaited callback for observing stateful-agent lifecycle events.
///
/// The agent applies an event to state first, then invokes listeners sequentially in registration
/// order. Each listener receives a clone of the active run's cancellation token. Listener completion
/// is part of run completion, including for [`AgentEvent::AgentEnd`].
pub type AgentListener =
    Arc<dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

/// Construction and runtime configuration for [`Agent`].
///
/// [`Agent::new`] clones the initial state and retains callback handles for later per-run snapshots.
/// Dedicated runtime setters can replace those handles only while the agent is idle.
#[derive(Clone)]
pub struct AgentConfig {
    /// Initial persistent state.
    ///
    /// Construction clears transient streaming, pending-call, and error fields.
    pub initial_state: AgentState,
    /// Agent-specific provider stream function.
    ///
    /// When absent, each run admission uses the installed process default.
    pub stream_fn: Option<Arc<dyn StreamFn>>,
    /// Transcript conversion used at each provider boundary.
    pub convert_to_llm: ConvertToLlm,
    /// Optional provider-boundary transcript transform.
    pub transform_context: Option<TransformContextHook>,
    /// Optional hook for blocking tool calls or mutating validated arguments.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Optional hook for explicitly overriding executed tool results.
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Optional post-turn graceful-stop predicate.
    pub should_stop_after_turn: Option<AgentShouldStopAfterTurnHook>,
    /// Optional legacy next-turn hook that receives only the active cancellation token.
    pub prepare_next_turn: Option<AgentPrepareNextTurnHook>,
    /// Optional context-aware next-turn hook.
    ///
    /// When both preparation hook fields are set, this context-aware hook takes precedence.
    pub prepare_next_turn_with_context: Option<AgentPrepareNextTurnWithContextHook>,
    /// Cache-affinity/session identifier mapped to `ChatOptions::prompt_cache_key`.
    ///
    /// If absent at construction, the initial chat option's prompt-cache key becomes this value.
    pub session_id: Option<String>,
    /// Initial steering-queue drain policy.
    pub steering_mode: QueueMode,
    /// Initial follow-up-queue drain policy.
    pub follow_up_mode: QueueMode,
    /// Tool-call batch execution policy.
    pub tool_execution: ToolExecutionMode,
    /// Base provider chat options.
    ///
    /// Per-run snapshots overwrite `prompt_cache_key` from [`Self::session_id`] and
    /// `reasoning_effort` from [`AgentState::thinking_level`].
    pub chat_options: ChatOptions,
}

/// Compatibility name matching the upstream TypeScript constructor documentation.
pub type AgentOptions = AgentConfig;

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("initial_state", &self.initial_state)
            .field("stream_fn", &self.stream_fn.is_some())
            .field("transform_context", &self.transform_context.is_some())
            .field("before_tool_call", &self.before_tool_call.is_some())
            .field("after_tool_call", &self.after_tool_call.is_some())
            .field(
                "should_stop_after_turn",
                &self.should_stop_after_turn.is_some(),
            )
            .field("prepare_next_turn", &self.prepare_next_turn.is_some())
            .field(
                "prepare_next_turn_with_context",
                &self.prepare_next_turn_with_context.is_some(),
            )
            .field("session_id", &self.session_id)
            .field("steering_mode", &self.steering_mode)
            .field("follow_up_mode", &self.follow_up_mode)
            .field("tool_execution", &self.tool_execution)
            .field("chat_options", &self.chat_options)
            .finish_non_exhaustive()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            initial_state: AgentState::default(),
            stream_fn: None,
            convert_to_llm: default_convert_to_llm(),
            transform_context: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            prepare_next_turn_with_context: None,
            session_id: None,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            tool_execution: ToolExecutionMode::Parallel,
            chat_options: ChatOptions::default(),
        }
    }
}

impl AgentConfig {
    /// Install an agent-specific provider stream function.
    pub fn with_stream_fn(mut self, stream_fn: Arc<dyn StreamFn>) -> Self {
        self.stream_fn = Some(stream_fn);
        self
    }

    /// Replace the state used to construct the agent.
    pub fn with_initial_state(mut self, state: AgentState) -> Self {
        self.initial_state = state;
        self
    }
}

/// Text, single-message, or message-batch input accepted by [`Agent::prompt`].
#[derive(Debug, Clone)]
pub enum PromptInput {
    /// Construct one user message from text followed by additional user-content parts.
    Text {
        /// Leading text content of the user message.
        text: String,
        /// Additional content parts appended after the text, typically images.
        images: Vec<UserContent>,
    },
    /// Append one already-constructed agent message.
    Message(AgentMessage),
    /// Append an ordered batch of already-constructed agent messages.
    Messages(Vec<AgentMessage>),
}

impl PromptInput {
    /// Construct text input with additional user-content parts.
    pub fn text_with_images(text: impl Into<String>, images: Vec<UserContent>) -> Self {
        Self::Text {
            text: text.into(),
            images,
        }
    }

    /// Normalize this input into the messages appended by a new run.
    pub fn into_messages(self) -> Vec<AgentMessage> {
        match self {
            Self::Text { text, images } => {
                let mut content = vec![UserContent::text(text)];
                content.extend(images);
                vec![AgentMessage::User(UserMessage::new(content))]
            }
            Self::Message(message) => vec![message],
            Self::Messages(messages) => messages,
        }
    }
}

impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        Self::Text {
            text: value.to_owned(),
            images: Vec::new(),
        }
    }
}

impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        Self::Text {
            text: value,
            images: Vec::new(),
        }
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(value: AgentMessage) -> Self {
        Self::Message(value)
    }
}

impl From<Vec<AgentMessage>> for PromptInput {
    fn from(value: Vec<AgentMessage>) -> Self {
        Self::Messages(value)
    }
}

enum LoopInvocation {
    Prompt {
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    },
    Continue,
}

struct PreparedRun {
    run: Arc<ActiveRun>,
    stream_fn: Arc<dyn StreamFn>,
    invocation: LoopInvocation,
}

/// Queue backing steering/follow-up injection.
#[derive(Debug, Clone)]
struct PendingMessageQueue {
    mode: QueueMode,
    messages: VecDeque<AgentMessage>,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            messages: VecDeque::new(),
        }
    }

    fn mode(&self) -> QueueMode {
        self.mode
    }

    fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push_back(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

struct ActiveRun {
    cancel: CancellationToken,
    settled: CancellationToken,
}

impl ActiveRun {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            settled: CancellationToken::new(),
        }
    }

    async fn wait_until_settled(&self) {
        self.settled.cancelled().await;
    }

    fn settle(&self) {
        self.settled.cancel();
    }
}

struct AgentInner {
    state: RwLock<AgentState>,
    config: RwLock<AgentConfig>,
    listeners: Mutex<BTreeMap<u64, AgentListener>>,
    next_listener_id: AtomicU64,
    steering: Mutex<PendingMessageQueue>,
    follow_up: Mutex<PendingMessageQueue>,
    active_run: Mutex<Option<Arc<ActiveRun>>>,
}

struct ActiveRunGuard {
    inner: Arc<AgentInner>,
    run: Arc<ActiveRun>,
    finished: bool,
}

impl ActiveRunGuard {
    fn new(inner: Arc<AgentInner>, run: Arc<ActiveRun>) -> Self {
        Self {
            inner,
            run,
            finished: false,
        }
    }

    fn finish(&mut self) {
        if !self.finished {
            finish_active_run(&self.inner, &self.run);
            self.finished = true;
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.run.cancel.cancel();
            finish_active_run(&self.inner, &self.run);
            self.finished = true;
        }
    }
}

fn finish_active_run(inner: &AgentInner, run: &Arc<ActiveRun>) {
    let mut active = inner
        .active_run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, run))
    {
        let mut state = inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        drop(state);
        *active = None;
    }
    drop(active);
    run.settle();
}

/// Stateful, cloneable facade over the low-level loop contract.
///
/// Clones share state, queues, subscriptions, configuration, and the active-run slot. At most one
/// prompt or continuation can be active across all clones.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Construct an agent from persistent state and runtime configuration.
    ///
    /// Transient state fields are reset to idle. A configured session id is copied to the chat
    /// options' prompt-cache key; otherwise an existing prompt-cache key initializes the session id.
    pub fn new(mut config: AgentConfig) -> Self {
        if let Some(session_id) = config.session_id.clone() {
            config.chat_options.prompt_cache_key = Some(session_id);
        } else {
            config.session_id = config.chat_options.prompt_cache_key.clone();
        }
        config.initial_state.is_streaming = false;
        config.initial_state.streaming_message = None;
        config.initial_state.pending_tool_calls.clear();
        config.initial_state.error_message = None;
        let steering_mode = config.steering_mode;
        let follow_up_mode = config.follow_up_mode;
        Self {
            inner: Arc::new(AgentInner {
                state: RwLock::new(config.initial_state.clone()),
                config: RwLock::new(config),
                listeners: Mutex::new(BTreeMap::new()),
                next_listener_id: AtomicU64::new(1),
                steering: Mutex::new(PendingMessageQueue::new(steering_mode)),
                follow_up: Mutex::new(PendingMessageQueue::new(follow_up_mode)),
                active_run: Mutex::new(None),
            }),
        }
    }

    /// Clone the agent's point-in-time observable state.
    ///
    /// The snapshot owns its message/tool vectors; tool implementations remain shared through
    /// [`Arc`] handles.
    pub fn state(&self) -> AgentState {
        self.inner
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the stored system prompt without emitting an event.
    ///
    /// A run uses the value present when it takes its initial state snapshot. For deterministic
    /// inputs, call this unguarded state setter while the agent is idle.
    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) {
        self.write_state().system_prompt = system_prompt.into();
    }

    /// Replace the stored model without emitting an event.
    ///
    /// A run uses the value present when it takes its initial state snapshot. For deterministic
    /// inputs, call this unguarded state setter while the agent is idle.
    pub fn set_model(&self, model: impl Into<ModelSpec>) {
        self.write_state().model = model.into();
    }

    /// Replace the stored reasoning request without emitting an event.
    ///
    /// A stateful run maps the snapshotted value to its chat options, including explicit
    /// [`ThinkingLevel::Budget`] values. For deterministic inputs, call this unguarded state setter
    /// while the agent is idle.
    pub fn set_thinking_level(&self, thinking_level: ThinkingLevel) {
        self.write_state().thinking_level = thinking_level;
    }

    /// Replace the stored tool set without emitting an event.
    ///
    /// A run uses the vector present when it takes its initial state snapshot. For deterministic
    /// inputs, call this unguarded state setter while the agent is idle.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.write_state().tools = tools;
    }

    /// Replace the committed transcript without emitting message events.
    ///
    /// A run uses the vector present when it takes its initial state snapshot. For deterministic
    /// transcript replacement, call this unguarded state setter while the agent is idle.
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.write_state().messages = messages;
    }

    /// Replace the provider stream function for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_stream_fn(&self, stream_fn: Arc<dyn StreamFn>) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.stream_fn = Some(stream_fn))
    }

    /// Replace the message-to-LLM converter for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_convert_to_llm(&self, convert_to_llm: ConvertToLlm) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.convert_to_llm = convert_to_llm)
    }

    /// Replace or clear the context transform hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_transform_context(
        &self,
        transform_context: Option<TransformContextHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.transform_context = transform_context)
    }

    /// Replace or clear the before-tool-call hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_before_tool_call(
        &self,
        before_tool_call: Option<BeforeToolCallHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.before_tool_call = before_tool_call)
    }

    /// Replace or clear the after-tool-call hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_after_tool_call(
        &self,
        after_tool_call: Option<AfterToolCallHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.after_tool_call = after_tool_call)
    }

    /// Replace or clear the graceful-stop hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_should_stop_after_turn(
        &self,
        should_stop_after_turn: Option<AgentShouldStopAfterTurnHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| {
            config.should_stop_after_turn = should_stop_after_turn;
        })
    }

    /// Replace or clear the legacy next-turn hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_prepare_next_turn(
        &self,
        prepare_next_turn: Option<AgentPrepareNextTurnHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.prepare_next_turn = prepare_next_turn)
    }

    /// Replace or clear the context-aware next-turn hook for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_prepare_next_turn_with_context(
        &self,
        prepare_next_turn: Option<AgentPrepareNextTurnWithContextHook>,
    ) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| {
            config.prepare_next_turn_with_context = prepare_next_turn;
        })
    }

    /// Replace the tool execution strategy for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active.
    pub fn set_tool_execution(&self, tool_execution: ToolExecutionMode) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| config.tool_execution = tool_execution)
    }

    /// Replace provider chat options for the next run.
    ///
    /// Runtime configuration may only be changed between runs; this returns
    /// [`AgentError::Busy`] while a prompt or continuation is active. An existing
    /// session id remains authoritative for `prompt_cache_key`; when no session id
    /// is set, a replacement option's cache key becomes the session id.
    pub fn set_chat_options(&self, mut chat_options: ChatOptions) -> Result<(), AgentError> {
        self.update_runtime_config(move |config| {
            if let Some(session_id) = config.session_id.clone() {
                chat_options.prompt_cache_key = Some(session_id);
            } else {
                config.session_id = chat_options.prompt_cache_key.clone();
            }
            config.chat_options = chat_options;
        })
    }

    /// Register an awaited event listener and return its RAII registration.
    ///
    /// Registration does not emit a state snapshot. For every later event, the agent updates state
    /// first and then awaits listeners in registration order. Retain the returned [`Subscription`]
    /// for as long as the callback should remain registered.
    pub fn subscribe(&self, listener: AgentListener) -> Subscription {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, listener);
        Subscription {
            agent: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// Register an async closure as an awaited event listener.
    ///
    /// This is the generic closure form of [`Self::subscribe`] and has identical ordering and RAII
    /// lifetime behavior.
    pub fn subscribe_fn<F, Fut>(&self, listener: F) -> Subscription
    where
        F: Fn(AgentEvent, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.subscribe(Arc::new(move |event, cancel| {
            Box::pin(listener(event, cancel))
        }))
    }

    /// Enqueue a steering message in FIFO order.
    ///
    /// Steering is polled before the initial assistant response and after continuing turns. The
    /// message does not enter [`AgentState::messages`] until the loop emits it, and the active
    /// [`QueueMode`] controls how many queued messages one poll drains.
    pub fn steer(&self, message: AgentMessage) {
        self.inner
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enqueue(message);
    }

    /// Enqueue a follow-up message in FIFO order.
    ///
    /// Follow-ups are polled when the loop would otherwise finish. The message does not enter
    /// [`AgentState::messages`] until emitted, and the active [`QueueMode`] controls how many queued
    /// messages one poll drains.
    pub fn follow_up(&self, message: AgentMessage) {
        self.inner
            .follow_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enqueue(message);
    }

    /// Remove all steering messages that have not yet been drained by the loop.
    pub fn clear_steering_queue(&self) {
        self.inner
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Remove all follow-up messages that have not yet been drained by the loop.
    pub fn clear_follow_up_queue(&self) {
        self.inner
            .follow_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Clear both pending-message queues.
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Return whether either queue currently contains an undrained message.
    pub fn has_queued_messages(&self) -> bool {
        self.inner
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_items()
            || self
                .inner
                .follow_up
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .has_items()
    }

    /// Return the steering queue's current drain policy.
    pub fn steering_mode(&self) -> QueueMode {
        self.inner
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mode()
    }

    /// Set the policy used by subsequent steering-queue drains.
    ///
    /// Existing queued messages are retained.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.inner
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_mode(mode);
    }

    /// Return the follow-up queue's current drain policy.
    pub fn follow_up_mode(&self) -> QueueMode {
        self.inner
            .follow_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mode()
    }

    /// Set the policy used by subsequent follow-up-queue drains.
    ///
    /// Existing queued messages are retained.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner
            .follow_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_mode(mode);
    }

    /// Clone the cache-affinity identifier used for provider request snapshots.
    pub fn session_id(&self) -> Option<String> {
        self.inner
            .config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id
            .clone()
    }

    /// Set or clear the cache-affinity id used by subsequent provider configuration snapshots.
    ///
    /// This also updates the stored chat options' `prompt_cache_key`. The setter is not guarded by
    /// [`AgentError::Busy`]; call it while idle when the next run must deterministically observe it.
    pub fn set_session_id(&self, session_id: Option<String>) {
        let mut config = self
            .inner
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        config.session_id = session_id.clone();
        config.chat_options.prompt_cache_key = session_id;
    }

    /// Clone the active run's cancellation token, or return `None` while idle.
    ///
    /// A cloned token remains usable after the run settles, but [`Self::signal`] stops returning it
    /// once the active-run slot is cleared.
    pub fn signal(&self) -> Option<CancellationToken> {
        self.inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|run| run.cancel.clone())
    }

    /// Request cooperative cancellation of the active run.
    ///
    /// This is a no-op while idle and does not wait for settlement; use [`Self::wait_for_idle`] when
    /// completion, including awaited listeners, is required.
    pub fn abort(&self) {
        if let Some(cancel) = self.signal() {
            cancel.cancel();
        }
    }

    /// Wait for the run active at the time of this call to settle.
    ///
    /// Settlement includes state updates and all awaited event listeners, including listeners for
    /// the final event. The method returns immediately if no run is active and does not wait for a
    /// future run admitted afterward.
    pub async fn wait_for_idle(&self) {
        let active = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(run) = active {
            run.wait_until_settled().await;
        }
    }

    /// Clear the transcript, transient run state, error text, and both message queues.
    ///
    /// The system prompt, model, thinking level, tools, runtime configuration, and listeners are
    /// preserved. Reset does not cancel or emit events; like the runtime-configuration setters it
    /// serializes with run admission and is rejected while a run is active.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Busy`] with [`BusyContext::Reset`] while a prompt or continuation is
    /// active; state and queues are left untouched.
    pub fn reset(&self) -> Result<(), AgentError> {
        let active = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(AgentError::Busy(BusyContext::Reset));
        }
        let mut state = self.write_state();
        state.messages.clear();
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.error_message = None;
        drop(state);
        self.clear_all_queues();
        drop(active);
        Ok(())
    }

    /// Start a new run by appending text, one message, or a message batch to the transcript.
    ///
    /// The low-level invocation runs from owned snapshots of agent state and guarded runtime
    /// configuration. This future resolves only after the run and its awaited listeners
    /// settle. Provider, tool, cancellation, and recovered loop failures are recorded in-band in
    /// state/events rather than returned from this method.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Busy`] if another run is active, or
    /// [`AgentError::NoDefaultStreamFn`] if neither this agent nor the process has a stream function.
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<(), AgentError> {
        let messages = input.into().into_messages();
        let (run, stream_fn) = self.begin_run()?;
        let mut guard = ActiveRunGuard::new(self.inner.clone(), run.clone());
        self.execute_run(
            run,
            stream_fn,
            LoopInvocation::Prompt {
                messages,
                skip_initial_steering_poll: false,
            },
        )
        .await;
        guard.finish();
        Ok(())
    }

    /// Start a new run with one user message containing text and additional content parts.
    ///
    /// The text part is first, followed by `images` in the supplied order. Admission, settlement,
    /// and errors are identical to [`Self::prompt`].
    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<UserContent>,
    ) -> Result<(), AgentError> {
        self.prompt(PromptInput::text_with_images(text, images))
            .await
    }

    /// Continue from the committed transcript without appending a direct prompt argument.
    ///
    /// A user, tool-result, or custom tail whose role is not `"assistant"` continues directly. At
    /// an assistant-role tail, continuation drains steering input first and then follow-up input; it
    /// fails if both queues are empty. The
    /// selected queue's [`QueueMode`] still controls how many messages are injected. Like
    /// [`Self::prompt`], this future includes awaited listener settlement and carries runtime
    /// outcomes in-band.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Busy`] for an active run, [`AgentError::EmptyContext`] for an empty
    /// transcript, [`AgentError::ContinueFromAssistant`] for an assistant tail with no queued input,
    /// or [`AgentError::NoDefaultStreamFn`] when no stream function is available.
    pub async fn continue_(&self) -> Result<(), AgentError> {
        let PreparedRun {
            run,
            stream_fn,
            invocation,
        } = self.begin_continuation()?;
        let mut guard = ActiveRunGuard::new(self.inner.clone(), run.clone());
        self.execute_run(run, stream_fn, invocation).await;
        guard.finish();
        Ok(())
    }

    fn begin_run(&self) -> Result<(Arc<ActiveRun>, Arc<dyn StreamFn>), AgentError> {
        let mut active = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(AgentError::Busy(BusyContext::Prompt));
        }
        let stream_fn = self.resolve_stream_fn()?;
        let run = self.activate_run(&mut active);
        Ok((run, stream_fn))
    }

    fn begin_continuation(&self) -> Result<PreparedRun, AgentError> {
        let mut active = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(AgentError::Busy(BusyContext::Continue));
        }

        let last = self
            .inner
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .last()
            .cloned()
            .ok_or(AgentError::EmptyContext)?;

        let (stream_fn, invocation) = if last.role() == "assistant" {
            let mut steering = self
                .inner
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if steering.has_items() {
                let stream_fn = self.resolve_stream_fn()?;
                let messages = steering.drain();
                (
                    stream_fn,
                    LoopInvocation::Prompt {
                        messages,
                        skip_initial_steering_poll: true,
                    },
                )
            } else {
                drop(steering);
                let mut follow_up = self
                    .inner
                    .follow_up
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !follow_up.has_items() {
                    return Err(AgentError::ContinueFromAssistant);
                }
                let stream_fn = self.resolve_stream_fn()?;
                let messages = follow_up.drain();
                (
                    stream_fn,
                    LoopInvocation::Prompt {
                        messages,
                        skip_initial_steering_poll: false,
                    },
                )
            }
        } else {
            (self.resolve_stream_fn()?, LoopInvocation::Continue)
        };

        let run = self.activate_run(&mut active);
        Ok(PreparedRun {
            run,
            stream_fn,
            invocation,
        })
    }

    fn activate_run(&self, active: &mut Option<Arc<ActiveRun>>) -> Arc<ActiveRun> {
        debug_assert!(active.is_none());
        let run = Arc::new(ActiveRun::new());
        {
            let mut state = self.write_state();
            state.is_streaming = true;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
            state.error_message = None;
        }
        *active = Some(run.clone());
        run
    }

    async fn execute_run(
        &self,
        run: Arc<ActiveRun>,
        stream_fn: Arc<dyn StreamFn>,
        invocation: LoopInvocation,
    ) {
        let agent = self.clone();
        let execution_run = run.clone();
        let execution = async move {
            let state = agent.state();
            let context = AgentContext {
                system_prompt: state.system_prompt.clone(),
                messages: state.messages.clone(),
                tools: state.tools.clone(),
            };
            let skip_initial_steering_poll = match &invocation {
                LoopInvocation::Prompt {
                    skip_initial_steering_poll,
                    ..
                } => *skip_initial_steering_poll,
                LoopInvocation::Continue => false,
            };
            let config =
                agent.create_loop_config(&state, &execution_run, skip_initial_steering_poll);
            let sink_agent = agent.clone();
            let sink_run = execution_run.clone();
            let mut sink = move |event| {
                let sink_agent = sink_agent.clone();
                let sink_run = sink_run.clone();
                async move { sink_agent.process_event(event, &sink_run).await }
            };

            match invocation {
                LoopInvocation::Prompt { messages, .. } => {
                    run_agent_loop(
                        messages,
                        context,
                        config,
                        &mut sink,
                        execution_run.cancel.clone(),
                        Some(stream_fn),
                    )
                    .await
                }
                LoopInvocation::Continue => {
                    run_agent_loop_continue(
                        context,
                        config,
                        &mut sink,
                        execution_run.cancel.clone(),
                        Some(stream_fn),
                    )
                    .await
                }
            }
        };

        match std::panic::AssertUnwindSafe(execution).catch_unwind().await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.handle_run_failure(error.to_string(), run.cancel.is_cancelled(), &run)
                    .await;
            }
            Err(payload) => {
                self.handle_run_failure(
                    panic_payload_message(payload.as_ref()),
                    run.cancel.is_cancelled(),
                    &run,
                )
                .await;
            }
        }
    }

    fn create_loop_config(
        &self,
        state: &AgentState,
        run: &Arc<ActiveRun>,
        skip_initial_steering_poll: bool,
    ) -> AgentLoopConfig {
        let runtime = self
            .inner
            .config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        let mut chat_options = runtime.chat_options.clone();
        chat_options.prompt_cache_key = runtime.session_id.clone();
        chat_options.reasoning_effort = state.thinking_level.reasoning_effort();

        let should_stop_after_turn = runtime.should_stop_after_turn.map(|hook| {
            let cancel = run.cancel.clone();
            Arc::new(move |context| hook(context, cancel.clone())) as crate::ShouldStopAfterTurnHook
        });

        let prepare_next_turn = if let Some(hook) = runtime.prepare_next_turn_with_context {
            let cancel = run.cancel.clone();
            Some(Arc::new(move |context| hook(context, cancel.clone()))
                as crate::PrepareNextTurnHook)
        } else if let Some(hook) = runtime.prepare_next_turn {
            let cancel = run.cancel.clone();
            Some(Arc::new(move |_context| hook(cancel.clone())) as crate::PrepareNextTurnHook)
        } else {
            None
        };

        let steering_inner = self.inner.clone();
        let skip_initial_poll = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        let get_steering_messages: crate::QueueMessagesHook = Arc::new(move || {
            let steering_inner = steering_inner.clone();
            let skip_initial_poll = skip_initial_poll.clone();
            Box::pin(async move {
                if skip_initial_poll.swap(false, Ordering::AcqRel) {
                    Vec::new()
                } else {
                    steering_inner
                        .steering
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .drain()
                }
            })
        });

        let follow_up_inner = self.inner.clone();
        let get_follow_up_messages: crate::QueueMessagesHook = Arc::new(move || {
            let follow_up_inner = follow_up_inner.clone();
            Box::pin(async move {
                follow_up_inner
                    .follow_up
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .drain()
            })
        });

        AgentLoopConfig {
            model: state.model.clone(),
            convert_to_llm: runtime.convert_to_llm,
            transform_context: runtime.transform_context,
            should_stop_after_turn,
            prepare_next_turn,
            get_steering_messages: Some(get_steering_messages),
            get_follow_up_messages: Some(get_follow_up_messages),
            before_tool_call: runtime.before_tool_call,
            after_tool_call: runtime.after_tool_call,
            tool_execution: runtime.tool_execution,
            chat_options,
        }
    }

    async fn handle_run_failure(&self, error: String, aborted: bool, run: &Arc<ActiveRun>) {
        let model = model_iden(&self.state().model);
        let reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        let mut failure = AssistantMessage::error(model, reason, error);
        failure.content = vec![AssistantContent::text("")];
        let failure = AgentMessage::Assistant(failure);

        self.process_event(
            AgentEvent::MessageStart {
                message: failure.clone(),
            },
            run,
        )
        .await;
        self.process_event(
            AgentEvent::MessageEnd {
                message: failure.clone(),
            },
            run,
        )
        .await;
        self.process_event(
            AgentEvent::TurnEnd {
                message: failure.clone(),
                tool_results: Vec::new(),
            },
            run,
        )
        .await;
        self.process_event(
            AgentEvent::AgentEnd {
                messages: vec![failure],
            },
            run,
        )
        .await;
    }

    async fn process_event(&self, event: AgentEvent, run: &Arc<ActiveRun>) {
        {
            let mut state = self.write_state();
            match &event {
                AgentEvent::MessageStart { message }
                | AgentEvent::MessageUpdate { message, .. } => {
                    state.streaming_message = Some(message.clone());
                }
                AgentEvent::MessageEnd { message } => {
                    state.streaming_message = None;
                    state.messages.push(message.clone());
                }
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    state.pending_tool_calls.insert(tool_call_id.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    state.pending_tool_calls.remove(tool_call_id);
                }
                // TS truthiness: an empty-string errorMessage does not populate state.
                AgentEvent::TurnEnd {
                    message: AgentMessage::Assistant(message),
                    ..
                } if message
                    .error_message
                    .as_deref()
                    .is_some_and(|m| !m.is_empty()) =>
                {
                    state.error_message = message.error_message.clone();
                }
                AgentEvent::AgentEnd { .. } => {
                    state.streaming_message = None;
                }
                _ => {}
            }
        }

        let mut cursor = None;
        loop {
            let next = {
                let listeners = self
                    .inner
                    .listeners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match cursor {
                    Some(id) => listeners
                        .range((std::ops::Bound::Excluded(id), std::ops::Bound::Unbounded))
                        .next(),
                    None => listeners.iter().next(),
                }
                .map(|(id, listener)| (*id, listener.clone()))
            };
            let Some((id, listener)) = next else {
                break;
            };
            cursor = Some(id);
            listener(event.clone(), run.cancel.clone()).await;
        }
    }

    fn resolve_stream_fn(&self) -> Result<Arc<dyn StreamFn>, AgentError> {
        self.inner
            .config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stream_fn
            .clone()
            .or_else(get_default_stream_fn)
            .ok_or(AgentError::NoDefaultStreamFn)
    }

    /// Serialize runtime updates with run admission. Both paths acquire `active_run`
    /// before `config`, so a successful update is wholly visible to the next run and
    /// can never partially affect a run that has already been admitted.
    fn update_runtime_config(
        &self,
        update: impl FnOnce(&mut AgentConfig),
    ) -> Result<(), AgentError> {
        let active = self
            .inner
            .active_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(AgentError::Busy(BusyContext::Other));
        }

        let mut config = self
            .inner
            .config
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut config);
        drop(config);
        drop(active);
        Ok(())
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, AgentState> {
        self.inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "agent loop panicked".to_owned()
    }
}

fn model_iden(model: &ModelSpec) -> ModelIden {
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

impl Default for Agent {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

/// RAII listener registration. Dropping it unsubscribes the callback.
#[must_use = "dropping the subscription immediately unregisters the listener"]
pub struct Subscription {
    agent: Weak<AgentInner>,
    id: u64,
}

impl Subscription {
    /// Consume this registration and unsubscribe immediately.
    ///
    /// This is equivalent to dropping the subscription. It prevents later callback selection but
    /// does not cancel a callback invocation that has already begun.
    pub fn unsubscribe(self) {}
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            agent
                .listeners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}
