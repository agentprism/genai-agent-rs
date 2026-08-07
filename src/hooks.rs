//! Infallible asynchronous extension points for loop decisions and tool results.
//!
//! Hook context values are owned snapshots of loop state. The sole borrowed callback input is
//! [`BeforeToolCallContext`], borrowed mutably so changes to its `args` field can become the
//! arguments that execute; changes to its other snapshot fields do not alter the loop. Other hooks
//! return explicit overrides or updates instead of mutating live state.
//!
//! These callback signatures intentionally have no error channel. A hook must encode its decision
//! in its return value and must not panic; unwinding is a contract violation (even where a
//! higher-level convenience boundary defensively translates the panic into an in-band failure).

use crate::{
    AgentContext, AgentMessage, AgentToolCall, AgentToolResult, AgentUsage, AssistantMessage,
    ThinkingLevel, ToolResultContent, ToolResultMessage,
};
use futures::future::BoxFuture;
use genai::ModelSpec;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Decision returned by a [`BeforeToolCallHook`].
///
/// Argument rewriting is performed separately by mutating [`BeforeToolCallContext::args`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BeforeToolCallResult {
    /// Whether to skip tool execution and synthesize an error tool result.
    pub block: bool,
    /// Error text used when [`Self::block`] is `true`.
    ///
    /// A default "Tool execution was blocked" message is used when this is `None`; a reason is
    /// ignored when the call is not blocked.
    pub reason: Option<String>,
}

/// Explicit field-by-field overrides returned by an [`AfterToolCallHook`].
///
/// Each `Some` value replaces the corresponding field from the executed result. `None` preserves
/// that field; these options therefore do not provide a way to clear an existing optional usage
/// value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AfterToolCallResult {
    /// Replacement result content.
    pub content: Option<Vec<ToolResultContent>>,
    /// Replacement application-defined result details.
    pub details: Option<Value>,
    /// Replacement error classification for emitted events and the tool-result message.
    pub is_error: Option<bool>,
    /// Replacement usage record stored on the tool result.
    pub usage: Option<AgentUsage>,
    /// Replacement request to terminate after the tool-call batch.
    ///
    /// A multi-call batch terminates the loop only when every finalized result requests it.
    pub terminate: Option<bool>,
}

/// Owned pre-execution snapshot borrowed by a [`BeforeToolCallHook`].
///
/// The executor consumes only [`Self::args`] after the hook returns. Mutating the assistant
/// message, tool call, or context fields changes this temporary snapshot but not live loop state.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    /// Assistant message that requested the call.
    pub assistant_message: AssistantMessage,
    /// Tool call being prepared.
    pub tool_call: AgentToolCall,
    /// Prepared and validated arguments that will be passed to the tool.
    ///
    /// If the call proceeds, hook mutations persist into execution and intentionally receive no
    /// second validation pass.
    pub args: Value,
    /// Conversation snapshot from before any results in this tool-call batch are appended.
    pub context: AgentContext,
}

/// Owned post-execution snapshot passed to an [`AfterToolCallHook`].
///
/// Mutating this value inside a hook does not itself change the finalized result. Return an
/// [`AfterToolCallResult`] to apply explicit overrides.
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    /// Assistant message that requested the call.
    pub assistant_message: AssistantMessage,
    /// Tool call that was executed.
    pub tool_call: AgentToolCall,
    /// Arguments actually passed to the tool, including before-hook mutations.
    pub args: Value,
    /// Tool result before after-hook overrides.
    pub result: AgentToolResult,
    /// Error classification before after-hook overrides.
    pub is_error: bool,
    /// Conversation snapshot from before any results in this tool-call batch are appended.
    pub context: AgentContext,
}

/// Owned post-turn snapshot used by stopping and next-turn preparation hooks.
#[derive(Debug, Clone)]
pub struct ShouldStopAfterTurnContext {
    /// Final assistant message for the completed turn.
    pub message: AssistantMessage,
    /// Finalized tool-result messages from the completed turn, in source order.
    pub tool_results: Vec<ToolResultMessage>,
    /// Current loop context at callback construction.
    ///
    /// Preparation sees the post-turn context; the stop predicate also sees any context replacement
    /// returned by the preceding preparation hook.
    pub context: AgentContext,
    /// Messages produced by this invocation so far, excluding the invocation's starting transcript.
    pub new_messages: Vec<AgentMessage>,
}

/// Post-turn snapshot passed to a [`PrepareNextTurnHook`].
pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// Explicit replacements to apply before a subsequent turn in the current invocation.
///
/// Every `None` field preserves the loop's current value. Updates affect later turns of this
/// invocation; they do not mutate the caller's original [`AgentContext`] or stateful [`crate::Agent`]
/// configuration.
#[derive(Debug, Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Complete replacement for the loop's current conversation context.
    pub context: Option<AgentContext>,
    /// Replacement model for later provider requests.
    pub model: Option<ModelSpec>,
    /// Replacement reasoning request for later provider requests.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Provider-boundary transcript transform.
///
/// The hook receives an owned clone of the current messages and the active cancellation token. Its
/// returned vector is converted for that provider request only; it does not replace loop context.
pub type TransformContextHook = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

/// Pre-execution tool hook.
///
/// Returning `None` (or a result with `block == false`) allows execution. When execution proceeds,
/// the borrowed context makes mutations to [`BeforeToolCallContext::args`] become the tool's
/// arguments without borrowing the snapshot across unrelated loop work.
pub type BeforeToolCallHook = Arc<
    dyn for<'a> Fn(
            &'a mut BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'a, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

/// Post-execution tool hook for explicit result overrides.
///
/// Returning `None` preserves the tool's result and error classification unchanged.
pub type AfterToolCallHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AfterToolCallResult>>
        + Send
        + Sync,
>;

/// Low-level post-turn predicate; returning `true` ends the current invocation.
pub type ShouldStopAfterTurnHook =
    Arc<dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync>;

/// Low-level post-turn hook for replacing context, model, or reasoning on later turns.
///
/// Returning `None` leaves all three values unchanged.
pub type PrepareNextTurnHook = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync,
>;

/// Asynchronous source of messages for a steering or follow-up poll.
///
/// An empty vector means no messages are queued at that poll.
pub type QueueMessagesHook = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

/// Stateful-agent post-turn predicate.
///
/// Returning `true` gracefully ends the run. The wrapper supplies the active run cancellation token
/// in addition to the owned post-turn snapshot.
pub type AgentShouldStopAfterTurnHook = Arc<
    dyn Fn(ShouldStopAfterTurnContext, CancellationToken) -> BoxFuture<'static, bool> + Send + Sync,
>;

/// Legacy stateful-agent next-turn hook, which receives only the active cancellation token.
///
/// Returning an update affects later turns in the active run; returning `None` preserves current
/// loop values.
pub type AgentPrepareNextTurnHook =
    Arc<dyn Fn(CancellationToken) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync>;

/// Context-aware stateful-agent next-turn hook.
///
/// It receives an owned post-turn snapshot plus the active cancellation token and may return
/// explicit replacements for later turns in the active run.
pub type AgentPrepareNextTurnWithContextHook = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;
