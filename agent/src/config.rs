//! Configuration primitives shared by the stateful agent and the low-level loop.
//!
//! [`AgentContext`] is the owned conversation snapshot consumed by one loop invocation, while
//! [`AgentLoopConfig`] selects provider-boundary transforms, lifecycle hooks, queue polling, and
//! tool execution policy. Callback fields are reference counted so a loop can snapshot them for
//! later turns without borrowing caller-owned configuration.

use crate::{
    AfterToolCallHook, AgentMessage, AgentTool, BeforeToolCallHook, ConvertToLlm, OnPayloadHook,
    OnResponseHook, PrepareNextTurnHook, QueueMessagesHook, ShouldStopAfterTurnHook,
    TransformContextHook, TryAfterToolCallHook, TryBeforeToolCallHook, default_convert_to_llm,
};
use genai::ModelSpec;
use genai::chat::{ChatOptions, ReasoningEffort};
use std::sync::Arc;

/// Execution policy for the tool calls contained in one assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Resolve the `reasoning_effort` chat option from a thinking level and an optional per-level
/// budget map.
///
/// This is the single resolution used both for a stateful agent's initial snapshot and for
/// next-turn [`crate::AgentLoopTurnUpdate::thinking_level`] updates, so both paths see identical
/// behavior:
///
/// - [`ThinkingLevel::Off`] yields `None` (no reasoning-effort option is set).
/// - An explicit [`ThinkingLevel::Budget`] always wins and is forwarded unchanged as
///   [`ReasoningEffort::Budget`], bypassing the map entirely.
/// - A named level resolves to [`ReasoningEffort::Budget`] when its budget entry is configured
///   (with `xhigh`/`max` clamping through the `high` entry, per pi-ai's `clampReasoning`), and
///   otherwise falls back to the named level's own reasoning effort.
pub fn resolve_reasoning_effort(
    level: ThinkingLevel,
    budgets: Option<&ThinkingBudgets>,
) -> Option<ReasoningEffort> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Budget(tokens) => Some(ReasoningEffort::Budget(tokens)),
        named => match budgets.and_then(|budgets| budgets.resolve(named)) {
            Some(budget) => Some(ReasoningEffort::Budget(budget)),
            None => named.reasoning_effort(),
        },
    }
}

/// Number of queued messages returned by one steering or follow-up poll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Per-named-level reasoning-token budgets used to resolve a [`ThinkingLevel`] to an explicit
/// provider budget.
///
/// This mirrors pi-ai's `ThinkingBudgets` map (`packages/ai/src/types.ts`). Two deliberate
/// differences keep the port honest:
///
/// - **No implicit default budget table.** pi-ai seeds a default `{minimal, low, medium, high}`
///   table and overlays the caller's map on top; here a level resolves to a budget only when its
///   own entry is explicitly configured. An unconfigured level therefore falls back to the named
///   reasoning effort rather than to a hardcoded token count.
/// - **No maxTokens-fitting step.** pi-ai additionally shrinks a resolved budget so it fits inside
///   the response ceiling (`adjustMaxTokensForThinking`). That step requires a model catalog
///   (context window / max output tokens), which this crate does not carry, so it is omitted.
///
/// Following pi-ai's `clampReasoning`, the extra-high and maximum levels resolve through the
/// [`Self::high`] entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ThinkingBudgets {
    /// Token budget for [`ThinkingLevel::Minimal`].
    pub minimal: Option<u32>,
    /// Token budget for [`ThinkingLevel::Low`].
    pub low: Option<u32>,
    /// Token budget for [`ThinkingLevel::Medium`].
    pub medium: Option<u32>,
    /// Token budget for [`ThinkingLevel::High`], and — via `clampReasoning` — for
    /// [`ThinkingLevel::XHigh`] and [`ThinkingLevel::Max`].
    pub high: Option<u32>,
}

impl ThinkingBudgets {
    /// Set the [`ThinkingLevel::Minimal`] token budget.
    pub const fn with_minimal(mut self, tokens: u32) -> Self {
        self.minimal = Some(tokens);
        self
    }

    /// Set the [`ThinkingLevel::Low`] token budget.
    pub const fn with_low(mut self, tokens: u32) -> Self {
        self.low = Some(tokens);
        self
    }

    /// Set the [`ThinkingLevel::Medium`] token budget.
    pub const fn with_medium(mut self, tokens: u32) -> Self {
        self.medium = Some(tokens);
        self
    }

    /// Set the [`ThinkingLevel::High`] token budget (also used for `xhigh`/`max` via `clampReasoning`).
    pub const fn with_high(mut self, tokens: u32) -> Self {
        self.high = Some(tokens);
        self
    }

    /// Resolve a named level to its configured token budget, or `None` when no entry applies.
    ///
    /// [`ThinkingLevel::Minimal`], [`ThinkingLevel::Low`], and [`ThinkingLevel::Medium`] map to
    /// their own fields. [`ThinkingLevel::High`], [`ThinkingLevel::XHigh`], and
    /// [`ThinkingLevel::Max`] all resolve through [`Self::high`], mirroring pi-ai's `clampReasoning`
    /// (`xhigh`/`max` → `high`). [`ThinkingLevel::Off`] and [`ThinkingLevel::Budget`] carry no
    /// named budget and return `None`.
    pub fn resolve(&self, level: ThinkingLevel) -> Option<u32> {
        match level {
            ThinkingLevel::Minimal => self.minimal,
            ThinkingLevel::Low => self.low,
            ThinkingLevel::Medium => self.medium,
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => self.high,
            ThinkingLevel::Off | ThinkingLevel::Budget(_) => None,
        }
    }
}

// `Transport` was relocated into the `genai` fork crate alongside `StreamRequest`, which carries
// it. Re-exported here so this module's public path (`crate::config::Transport`), the crate-root
// re-export, and this file's own unqualified `Transport` references stay unchanged.
pub use genai::stream_fn::Transport;

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
/// Callback fields are [`Arc`]-backed and can be cloned between turns. Legacy hooks have
/// infallible signatures; they must communicate their documented decisions through return values
/// rather than panic. The fallible tool channels ([`TryBeforeToolCallHook`]/
/// [`TryAfterToolCallHook`]) convert their errors into in-band error tool results.
#[derive(Clone)]
#[non_exhaustive]
pub struct AgentLoopConfig {
    /// Model used for provider requests unless a prepare-next-turn update replaces it.
    pub model: ModelSpec,
    /// Optional session identifier forwarded onto each [`crate::StreamRequest`]
    /// ([`crate::StreamRequest::session_id`]).
    ///
    /// The loop forwards the value without interpreting it; honoring it (for example for
    /// cache-affinity routing) is a stream-function concern. It is independent of
    /// `ChatOptions::prompt_cache_key`: setting or clearing it never writes that field.
    pub session_id: Option<String>,
    /// Optional per-named-level reasoning-token budgets.
    ///
    /// A prepare-next-turn [`ThinkingLevel`] update resolves through this map via
    /// [`resolve_reasoning_effort`], exactly like the stateful agent's initial snapshot. `None`
    /// leaves named levels mapped to their named reasoning efforts.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Optional maximum number of provider-handshake retries forwarded onto each
    /// [`crate::StreamRequest`].
    ///
    /// The loop performs no retries itself; honoring the value is a stream-function concern.
    /// [`crate::GenaiStreamFn`] treats it as a per-request override of its construction-time
    /// [`crate::RetryPolicy`].
    pub max_retries: Option<u32>,
    /// Optional cap, in milliseconds, on a *server-requested* retry delay, forwarded onto each
    /// [`crate::StreamRequest`].
    ///
    /// The loop performs no retries itself; honoring the value is a stream-function concern.
    /// [`crate::GenaiStreamFn`] treats it as a per-request override of its construction-time
    /// [`crate::RetryPolicy`].
    pub max_retry_delay_ms: Option<u64>,
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
    /// Optional legacy infallible pre-execution hook for blocking a call or mutating its
    /// validated arguments.
    ///
    /// When [`Self::try_before_tool_call`] is also set, the fallible hook takes precedence and this
    /// hook is never invoked.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Optional legacy infallible post-execution hook for explicitly overriding result fields.
    ///
    /// When [`Self::try_after_tool_call`] is also set, the fallible hook takes precedence and this
    /// hook is never invoked.
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Optional fallible pre-execution hook.
    ///
    /// An `Err` skips execution and becomes the call's in-band error tool result. Takes precedence
    /// over [`Self::before_tool_call`] when both are set; the two are never both invoked for one
    /// call. See [`TryBeforeToolCallHook`].
    pub try_before_tool_call: Option<TryBeforeToolCallHook>,
    /// Optional fallible post-execution hook.
    ///
    /// An `Err` replaces the completed result with an in-band error tool result. Takes precedence
    /// over [`Self::after_tool_call`] when both are set; the two are never both invoked for one
    /// call. See [`TryAfterToolCallHook`].
    pub try_after_tool_call: Option<TryAfterToolCallHook>,
    /// Execution policy for each assistant message's tool-call batch.
    pub tool_execution: ToolExecutionMode,
    /// Preferred provider transport advisory forwarded onto each [`crate::StreamRequest`].
    ///
    /// The loop copies this value; it does not interpret it. Honoring it is a stream-function
    /// concern, and the SSE-only [`crate::GenaiStreamFn`] ignores it.
    pub transport: Transport,
    /// Optional pre-send payload hook forwarded onto each [`crate::StreamRequest`].
    ///
    /// The loop forwards the handle without invoking it; honoring it is a stream-function
    /// concern (see [`OnPayloadHook`] for which built-in stream functions apply it where).
    pub on_payload: Option<OnPayloadHook>,
    /// Optional response observation hook forwarded onto each [`crate::StreamRequest`].
    ///
    /// The loop forwards the handle without invoking it; honoring it is a stream-function
    /// concern (see [`OnResponseHook`] for which built-in stream functions apply it where).
    pub on_response: Option<OnResponseHook>,
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
            .field("try_before_tool_call", &self.try_before_tool_call.is_some())
            .field("try_after_tool_call", &self.try_after_tool_call.is_some())
            .field("tool_execution", &self.tool_execution)
            .field("session_id", &self.session_id)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("transport", &self.transport)
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
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
            try_before_tool_call: None,
            try_after_tool_call: None,
            tool_execution: ToolExecutionMode::Parallel,
            transport: Transport::Auto,
            on_payload: None,
            on_response: None,
            session_id: None,
            thinking_budgets: None,
            max_retries: None,
            max_retry_delay_ms: None,
            chat_options: ChatOptions::default(),
        }
    }

    /// Set the session identifier forwarded onto each stream request (see [`Self::session_id`]).
    ///
    /// This never writes `ChatOptions::prompt_cache_key`; the two are independent.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the per-named-level reasoning-token budgets used for next-turn thinking updates.
    pub fn with_thinking_budgets(mut self, thinking_budgets: ThinkingBudgets) -> Self {
        self.thinking_budgets = Some(thinking_budgets);
        self
    }

    /// Set the maximum number of provider-handshake retries forwarded onto each stream request.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Set the server-requested retry-delay cap (milliseconds) forwarded onto each stream request.
    pub fn with_max_retry_delay_ms(mut self, max_retry_delay_ms: u64) -> Self {
        self.max_retry_delay_ms = Some(max_retry_delay_ms);
        self
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

    /// Replace the preferred provider transport advisory.
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Install the provider-boundary transcript transform.
    pub fn with_transform_context(mut self, transform_context: TransformContextHook) -> Self {
        self.transform_context = Some(transform_context);
        self
    }

    /// Install the post-turn graceful-stop predicate.
    pub fn with_should_stop_after_turn(
        mut self,
        should_stop_after_turn: ShouldStopAfterTurnHook,
    ) -> Self {
        self.should_stop_after_turn = Some(should_stop_after_turn);
        self
    }

    /// Install the post-turn context/model/reasoning preparation hook.
    pub fn with_prepare_next_turn(mut self, prepare_next_turn: PrepareNextTurnHook) -> Self {
        self.prepare_next_turn = Some(prepare_next_turn);
        self
    }

    /// Install the steering-message source polled before the initial response and between turns.
    pub fn with_get_steering_messages(mut self, get_steering_messages: QueueMessagesHook) -> Self {
        self.get_steering_messages = Some(get_steering_messages);
        self
    }

    /// Install the follow-up-message source polled when the loop would otherwise finish.
    pub fn with_get_follow_up_messages(
        mut self,
        get_follow_up_messages: QueueMessagesHook,
    ) -> Self {
        self.get_follow_up_messages = Some(get_follow_up_messages);
        self
    }

    /// Install the legacy infallible pre-execution tool hook.
    ///
    /// [`Self::with_try_before_tool_call`] takes precedence when both are installed.
    pub fn with_before_tool_call(mut self, before_tool_call: BeforeToolCallHook) -> Self {
        self.before_tool_call = Some(before_tool_call);
        self
    }

    /// Install the legacy infallible post-execution tool hook.
    ///
    /// [`Self::with_try_after_tool_call`] takes precedence when both are installed.
    pub fn with_after_tool_call(mut self, after_tool_call: AfterToolCallHook) -> Self {
        self.after_tool_call = Some(after_tool_call);
        self
    }

    /// Install the fallible pre-execution tool hook.
    ///
    /// Takes precedence over [`Self::with_before_tool_call`] when both are installed; the two are
    /// never both invoked for one call. An `Err` skips execution and becomes the call's in-band
    /// error tool result.
    pub fn with_try_before_tool_call(
        mut self,
        try_before_tool_call: TryBeforeToolCallHook,
    ) -> Self {
        self.try_before_tool_call = Some(try_before_tool_call);
        self
    }

    /// Install the fallible post-execution tool hook.
    ///
    /// Takes precedence over [`Self::with_after_tool_call`] when both are installed; the two are
    /// never both invoked for one call. An `Err` replaces the completed result with an in-band
    /// error tool result.
    pub fn with_try_after_tool_call(mut self, try_after_tool_call: TryAfterToolCallHook) -> Self {
        self.try_after_tool_call = Some(try_after_tool_call);
        self
    }

    /// Install the pre-send payload hook forwarded onto each stream request.
    pub fn with_on_payload(mut self, on_payload: OnPayloadHook) -> Self {
        self.on_payload = Some(on_payload);
        self
    }

    /// Install the response observation hook forwarded onto each stream request.
    pub fn with_on_response(mut self, on_response: OnResponseHook) -> Self {
        self.on_response = Some(on_response);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_budgets_resolve_named_levels_and_clamp_xhigh_and_max() {
        let budgets = ThinkingBudgets {
            minimal: Some(100),
            low: Some(200),
            medium: Some(300),
            high: Some(400),
        };
        assert_eq!(budgets.resolve(ThinkingLevel::Minimal), Some(100));
        assert_eq!(budgets.resolve(ThinkingLevel::Low), Some(200));
        assert_eq!(budgets.resolve(ThinkingLevel::Medium), Some(300));
        assert_eq!(budgets.resolve(ThinkingLevel::High), Some(400));
        // xhigh and max clamp through the high entry, mirroring pi-ai's clampReasoning.
        assert_eq!(budgets.resolve(ThinkingLevel::XHigh), Some(400));
        assert_eq!(budgets.resolve(ThinkingLevel::Max), Some(400));
        // Off and explicit budgets carry no named budget.
        assert_eq!(budgets.resolve(ThinkingLevel::Off), None);
        assert_eq!(budgets.resolve(ThinkingLevel::Budget(999)), None);
    }

    #[test]
    fn thinking_budgets_resolve_returns_none_for_unconfigured_named_levels() {
        // No implicit default budget table: an unconfigured entry stays None.
        let budgets = ThinkingBudgets {
            high: Some(400),
            ..ThinkingBudgets::default()
        };
        assert_eq!(budgets.resolve(ThinkingLevel::Minimal), None);
        assert_eq!(budgets.resolve(ThinkingLevel::Low), None);
        assert_eq!(budgets.resolve(ThinkingLevel::Medium), None);
        assert_eq!(budgets.resolve(ThinkingLevel::High), Some(400));
    }

    #[test]
    fn transport_defaults_to_auto() {
        assert_eq!(Transport::default(), Transport::Auto);
    }

    #[test]
    fn resolve_reasoning_effort_prefers_configured_budgets_and_clamps() {
        let budgets = ThinkingBudgets::default().with_low(200).with_high(400);
        let resolve = |level| resolve_reasoning_effort(level, Some(&budgets));

        assert!(resolve(ThinkingLevel::Off).is_none());
        assert!(
            matches!(
                resolve(ThinkingLevel::Budget(64)),
                Some(ReasoningEffort::Budget(64))
            ),
            "an explicit budget always bypasses the map"
        );
        assert!(matches!(
            resolve(ThinkingLevel::Low),
            Some(ReasoningEffort::Budget(200))
        ));
        assert!(matches!(
            resolve(ThinkingLevel::High),
            Some(ReasoningEffort::Budget(400))
        ));
        assert!(
            matches!(
                resolve(ThinkingLevel::Max),
                Some(ReasoningEffort::Budget(400))
            ),
            "max clamps through the high entry"
        );
        assert!(
            matches!(
                resolve(ThinkingLevel::Medium),
                Some(ReasoningEffort::Medium)
            ),
            "an unconfigured level falls back to its named reasoning effort"
        );
    }

    #[test]
    fn resolve_reasoning_effort_without_budgets_uses_named_efforts() {
        assert!(resolve_reasoning_effort(ThinkingLevel::Off, None).is_none());
        assert!(matches!(
            resolve_reasoning_effort(ThinkingLevel::High, None),
            Some(ReasoningEffort::High)
        ));
        assert!(matches!(
            resolve_reasoning_effort(ThinkingLevel::Budget(64), None),
            Some(ReasoningEffort::Budget(64))
        ));
    }

    #[test]
    fn agent_loop_config_debug_redacts_closures_and_shows_scalar_fields() {
        let config = AgentLoopConfig::new(
            ModelSpec::from_iden(crate::assistant::unknown_model_iden()),
            default_convert_to_llm(),
        )
        .with_session_id("debug-session")
        .with_max_retries(3)
        .with_max_retry_delay_ms(1_250)
        .with_thinking_budgets(ThinkingBudgets::default().with_high(400))
        .with_try_before_tool_call(std::sync::Arc::new(|_context, _cancel| {
            Box::pin(async move { Ok(None) })
        }))
        .with_try_after_tool_call(std::sync::Arc::new(|_context, _cancel| {
            Box::pin(async move { Ok(None) })
        }));

        let debug = format!("{config:?}");
        assert!(
            debug.contains("session_id: Some(\"debug-session\")"),
            "{debug}"
        );
        assert!(debug.contains("max_retries: Some(3)"), "{debug}");
        assert!(debug.contains("max_retry_delay_ms: Some(1250)"), "{debug}");
        assert!(debug.contains("try_before_tool_call: true"), "{debug}");
        assert!(debug.contains("try_after_tool_call: true"), "{debug}");
        assert!(
            !debug.contains("Fn("),
            "closures are redacted to presence flags: {debug}"
        );
    }

    #[test]
    fn transport_serde_uses_typescript_kebab_case_spellings() {
        for (transport, spelling) in [
            (Transport::Sse, "\"sse\""),
            (Transport::Websocket, "\"websocket\""),
            (Transport::WebsocketCached, "\"websocket-cached\""),
            (Transport::Auto, "\"auto\""),
        ] {
            assert_eq!(serde_json::to_string(&transport).unwrap(), spelling);
            assert_eq!(
                serde_json::from_str::<Transport>(spelling).unwrap(),
                transport
            );
        }
    }
}
