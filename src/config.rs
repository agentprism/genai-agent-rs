//! Configuration primitives shared by the stateful agent and the low-level loop.
//!
//! [`AgentContext`] is the owned conversation snapshot consumed by one loop invocation, while
//! [`AgentLoopConfig`] selects provider-boundary transforms, lifecycle hooks, queue polling, and
//! tool execution policy. Callback fields are reference counted so a loop can snapshot them for
//! later turns without borrowing caller-owned configuration.

use crate::{
    AfterToolCallHook, AgentMessage, AgentTool, BeforeToolCallHook, ConvertToLlm,
    PrepareNextTurnHook, QueueMessagesHook, ShouldStopAfterTurnHook, TransformContextHook,
    default_convert_to_llm,
};
use genai::ModelSpec;
use genai::chat::{ChatOptions, ReasoningEffort};
use std::sync::Arc;

/// Execution policy for the tool calls contained in one assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// Prepare and execute calls one at a time in source order.
    Sequential,
    /// Execute eligible calls concurrently after source-ordered preflight.
    ///
    /// A tool whose own execution mode is [`ToolExecutionMode::Sequential`] makes its entire
    /// assistant-message batch sequential.
    #[default]
    Parallel,
}

/// Number of queued messages returned by one steering or follow-up poll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Drain every message currently in the queue, preserving FIFO order.
    All,
    /// Drain at most the oldest message on each poll.
    #[default]
    OneAtATime,
}

/// Provider reasoning intensity requested for an assistant response.
///
/// Named levels and explicit budgets are requests: their support and interpretation remain
/// provider-specific.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingLevel {
    /// Do not set a reasoning-effort option.
    #[default]
    Off,
    /// Request the provider's minimal named reasoning effort.
    Minimal,
    /// Request low reasoning effort.
    Low,
    /// Request medium reasoning effort.
    Medium,
    /// Request high reasoning effort.
    High,
    /// Request the provider's extra-high reasoning effort.
    XHigh,
    /// Request the provider's maximum named reasoning effort.
    Max,
    /// Request an explicit provider-specific reasoning-token budget.
    ///
    /// The contained token count is forwarded unchanged as [`ReasoningEffort::Budget`].
    Budget(u32),
}

impl ThinkingLevel {
    /// Convert this level to the corresponding provider request option.
    ///
    /// [`ThinkingLevel::Off`] returns `None`; every other variant maps directly to a
    /// [`ReasoningEffort`] variant.
    pub fn reasoning_effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(ReasoningEffort::Minimal),
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::XHigh => Some(ReasoningEffort::XHigh),
            Self::Max => Some(ReasoningEffort::Max),
            Self::Budget(tokens) => Some(ReasoningEffort::Budget(tokens)),
        }
    }
}

/// Owned conversation snapshot passed into a low-level loop invocation.
///
/// The loop mutates its private copy as messages are produced. Hook contexts receive further
/// value snapshots, so changing one of those snapshots does not mutate a caller's original
/// context.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// System instruction sent with each provider request.
    pub system_prompt: String,
    /// Ordered transcript available at the start of the invocation.
    pub messages: Vec<AgentMessage>,
    /// Tools available for calls in this invocation.
    ///
    /// Cloning a context clones these [`Arc`] handles rather than the tool implementations.
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages", &self.messages)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.spec())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl AgentContext {
    /// Create an empty transcript and tool set with the given system prompt.
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            ..Self::default()
        }
    }

    /// Replace the starting transcript.
    pub fn with_messages(mut self, messages: Vec<AgentMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Replace the tools available to the loop.
    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }
}

/// Configuration snapshot for a low-level loop invocation.
///
/// Callback fields are [`Arc`]-backed and can be cloned between turns. Hooks have infallible
/// signatures; they must communicate their documented decisions through return values rather than
/// panic.
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// Model used for provider requests unless a prepare-next-turn update replaces it.
    pub model: ModelSpec,
    /// Conversion from the widened agent transcript to provider-compatible messages.
    ///
    /// The loop invokes this once per provider request, after [`Self::transform_context`].
    pub convert_to_llm: ConvertToLlm,
    /// Optional provider-boundary transcript transform.
    ///
    /// Its returned messages are passed to [`Self::convert_to_llm`] for that request only; they do
    /// not replace the loop's stored [`AgentContext`].
    pub transform_context: Option<TransformContextHook>,
    /// Optional post-turn predicate that ends the invocation when it returns `true`.
    ///
    /// It runs after [`Self::prepare_next_turn`], if present.
    pub should_stop_after_turn: Option<ShouldStopAfterTurnHook>,
    /// Optional post-turn hook for explicit context, model, or reasoning updates.
    pub prepare_next_turn: Option<PrepareNextTurnHook>,
    /// Optional source of steering messages.
    ///
    /// The loop polls it before the initial assistant response and after each continuing turn.
    pub get_steering_messages: Option<QueueMessagesHook>,
    /// Optional source of follow-up messages, polled when the loop would otherwise finish.
    pub get_follow_up_messages: Option<QueueMessagesHook>,
    /// Optional pre-execution hook for blocking a call or mutating its validated arguments.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Optional post-execution hook for explicitly overriding result fields.
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Execution policy for each assistant message's tool-call batch.
    pub tool_execution: ToolExecutionMode,
    /// Provider options cloned into each stream request.
    pub chat_options: ChatOptions,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model)
            .field("transform_context", &self.transform_context.is_some())
            .field(
                "should_stop_after_turn",
                &self.should_stop_after_turn.is_some(),
            )
            .field("prepare_next_turn", &self.prepare_next_turn.is_some())
            .field(
                "get_steering_messages",
                &self.get_steering_messages.is_some(),
            )
            .field(
                "get_follow_up_messages",
                &self.get_follow_up_messages.is_some(),
            )
            .field("before_tool_call", &self.before_tool_call.is_some())
            .field("after_tool_call", &self.after_tool_call.is_some())
            .field("tool_execution", &self.tool_execution)
            .field("chat_options", &self.chat_options)
            .finish_non_exhaustive()
    }
}

impl AgentLoopConfig {
    /// Create a configuration with parallel tools, default chat options, and no optional hooks.
    pub fn new(model: impl Into<ModelSpec>, convert_to_llm: ConvertToLlm) -> Self {
        Self {
            model: model.into(),
            convert_to_llm,
            transform_context: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            tool_execution: ToolExecutionMode::Parallel,
            chat_options: ChatOptions::default(),
        }
    }

    /// Replace the provider chat options.
    pub fn with_chat_options(mut self, options: ChatOptions) -> Self {
        self.chat_options = options;
        self
    }

    /// Replace the tool-call batch execution policy.
    pub fn with_tool_execution(mut self, mode: ToolExecutionMode) -> Self {
        self.tool_execution = mode;
        self
    }
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self::new(
            ModelSpec::from_iden(crate::assistant::unknown_model_iden()),
            default_convert_to_llm(),
        )
    }
}
