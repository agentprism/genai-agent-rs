//! Infallible asynchronous extension points for loop decisions and tool results.
//!
//! Hook context values are owned snapshots of loop state. The sole borrowed callback input is
//! [`BeforeToolCallContext`], borrowed mutably so changes to its `args` field can become the
//! arguments that execute; changes to its other snapshot fields do not alter the loop. Other hooks
//! return explicit overrides or updates instead of mutating live state.
//!
//! These legacy callback signatures intentionally have no error channel. A hook must encode its
//! decision in its return value and must not panic; unwinding is a contract violation (even where a
//! higher-level convenience boundary defensively translates the panic into an in-band failure).
//!
//! The tool-call channels additionally have opt-in fallible forms, [`TryBeforeToolCallHook`] and
//! [`TryAfterToolCallHook`], whose `Err` returns become ordinary in-band error tool results. This
//! mirrors pi, where a `beforeToolCall`/`afterToolCall` throw is caught by the loop and converted
//! into the call's error result. Each channel is resolved exactly once per tool call: a configured
//! fallible hook takes precedence over the legacy hook, and the two are never both invoked for one
//! call.

use crate::{
    AgentContext, AgentMessage, AgentToolCall, AgentToolResult, AgentUsage, AssistantMessage,
    ThinkingLevel, ToolHookError, ToolResultContent, ToolResultMessage,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use genai::ModelSpec;
use genai::chat::ChatMessage;
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
    /// A default "Tool execution was blocked" message is used when this is `None` or empty; a
    /// reason is ignored when the call is not blocked.
    pub reason: Option<String>,
    /// Whether a blocked call's synthesized error result requests loop termination.
    ///
    /// Only meaningful when [`Self::block`] is `true`. A multi-call batch still terminates only
    /// when every finalized result requests it.
    pub terminate: bool,
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

/// Fallible form of the pre-execution tool hook.
///
/// The `Ok` contract matches [`BeforeToolCallHook`] exactly: `Ok(None)` (or a result with
/// `block == false`) allows execution, and mutations to [`BeforeToolCallContext::args`] become the
/// tool's arguments. An `Err` return skips validation/execution and becomes the call's in-band
/// error tool result — the [`ToolHookError`] display text is the result text, mirroring pi's
/// `error.message` propagation for a thrown `beforeToolCall`. Unlike a blocked call, a failed
/// before-hook does not request batch termination.
///
/// When a configuration carries both this hook and the legacy [`BeforeToolCallHook`], only the
/// fallible hook runs for each call (deterministic precedence; never both).
pub type TryBeforeToolCallHook = Arc<
    dyn for<'a> Fn(
            &'a mut BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'a, Result<Option<BeforeToolCallResult>, ToolHookError>>
        + Send
        + Sync,
>;

/// Fallible form of the post-execution tool hook.
///
/// The `Ok` contract matches [`AfterToolCallHook`] exactly. An `Err` return replaces the completed
/// tool result with an in-band error tool result whose text is the [`ToolHookError`] display text
/// and whose error classification is `true`, mirroring pi's catch around `afterToolCall`: the
/// replacement discards the executed result's content, details, usage, and termination request.
/// Tool side effects are **not** rolled back — execution already happened; only the model-visible
/// result is replaced.
///
/// When a configuration carries both this hook and the legacy [`AfterToolCallHook`], only the
/// fallible hook runs for each call (deterministic precedence; never both).
pub type TryAfterToolCallHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Option<AfterToolCallResult>, ToolHookError>>
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

// `StreamResponseInfo`, `OnPayloadHook`, and `OnResponseHook` were relocated into the `genai` fork
// crate alongside the stream-function contract that consumes them. Re-exported here so this
// module's public paths (`crate::hooks::{StreamResponseInfo, OnPayloadHook, OnResponseHook}`) and
// their crate-root re-exports stay unchanged.
pub use genai::stream_fn::{OnPayloadHook, OnResponseHook, StreamResponseInfo};

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

// ---------------------------------------------------------------------------
// Object-safe trait mirrors (Layer A1) — foreign-friendly hook surface.
// ---------------------------------------------------------------------------
//
// Each `Arc<dyn Fn…>` hook alias above has an object-safe `#[async_trait]` trait counterpart here.
// These traits carry no foreign-hostile constructs: no closures, no borrowed callback arguments,
// and no higher-ranked lifetimes. A host (Swift/Kotlin via UniFFI, or a plain Rust struct) can
// implement the trait; the agent adapts `Arc<dyn Trait>` back into the existing closure alias and
// registers it through the same `Busy`-guarded setter the Rust-native closure form uses (see the
// `set_*_object` methods on [`crate::Agent`]). The closure aliases remain the primary Rust API;
// these traits are strictly additive.

/// Owned result of a [`BeforeToolCall`] (or [`TryBeforeToolCall`]) trait invocation.
///
/// The closure-form [`BeforeToolCallHook`] mutates [`BeforeToolCallContext::args`] in place across a
/// borrowed `&mut` context. A trait callback cannot hold that mutable borrow across the boundary, so
/// the trait mirror takes an **owned** context and returns this **owned** outcome; the adapter writes
/// `args` back into the loop's borrowed context (owned-in / owned-out equivalent of the in-place
/// mutation) and then returns `decision`.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallOutcome {
    /// Rewritten tool arguments to execute with. `None` leaves the prepared args unchanged.
    pub args: Option<Value>,
    /// Block/allow decision, identical to the closure form's return.
    pub decision: Option<BeforeToolCallResult>,
}

/// Object-safe mirror of [`TransformContextHook`].
///
/// Register with [`crate::Agent::set_transform_context_object`].
#[async_trait]
pub trait TransformContext: Send + Sync {
    /// Transform the transcript for a single provider request. See [`TransformContextHook`].
    async fn transform(
        &self,
        messages: Vec<AgentMessage>,
        cancel: CancellationToken,
    ) -> Vec<AgentMessage>;
}

/// Object-safe mirror of the legacy infallible [`BeforeToolCallHook`].
///
/// Takes an owned [`BeforeToolCallContext`] and returns an owned [`BeforeToolCallOutcome`]; the
/// adapter writes any returned `args` back into the loop's borrowed context before returning the
/// `decision` (the `&mut` borrow bridge). Register with
/// [`crate::Agent::set_before_tool_call_object`].
#[async_trait]
pub trait BeforeToolCall: Send + Sync {
    /// Inspect the prepared call; optionally rewrite `args` and/or return a block/allow decision.
    async fn before(
        &self,
        ctx: BeforeToolCallContext,
        cancel: CancellationToken,
    ) -> BeforeToolCallOutcome;
}

/// Object-safe mirror of the legacy infallible [`AfterToolCallHook`].
///
/// Register with [`crate::Agent::set_after_tool_call_object`].
#[async_trait]
pub trait AfterToolCall: Send + Sync {
    /// Optionally override the executed result. `None` preserves it unchanged.
    async fn after(
        &self,
        ctx: AfterToolCallContext,
        cancel: CancellationToken,
    ) -> Option<AfterToolCallResult>;
}

/// Object-safe mirror of the fallible [`TryBeforeToolCallHook`].
///
/// Like [`BeforeToolCall`], but `Err` skips execution and becomes the call's in-band error tool
/// result. On `Ok`, the adapter writes any returned `args` back into the borrowed context and returns
/// the `decision`. Register with [`crate::Agent::set_try_before_tool_call_object`].
#[async_trait]
pub trait TryBeforeToolCall: Send + Sync {
    /// Fallible pre-execution inspection; `Err` becomes the call's in-band error tool result.
    async fn before(
        &self,
        ctx: BeforeToolCallContext,
        cancel: CancellationToken,
    ) -> Result<BeforeToolCallOutcome, ToolHookError>;
}

/// Object-safe mirror of the fallible [`TryAfterToolCallHook`].
///
/// Register with [`crate::Agent::set_try_after_tool_call_object`].
#[async_trait]
pub trait TryAfterToolCall: Send + Sync {
    /// Fallible post-execution override; `Err` replaces the result with an in-band error result.
    async fn after(
        &self,
        ctx: AfterToolCallContext,
        cancel: CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolHookError>;
}

/// Object-safe mirror of the stateful-agent [`AgentShouldStopAfterTurnHook`].
///
/// Register with [`crate::Agent::set_should_stop_after_turn_object`].
#[async_trait]
pub trait ShouldStopAfterTurn: Send + Sync {
    /// Returning `true` gracefully ends the current run after the completed turn.
    async fn should_stop(
        &self,
        ctx: ShouldStopAfterTurnContext,
        cancel: CancellationToken,
    ) -> bool;
}

/// Object-safe mirror of the context-aware [`AgentPrepareNextTurnWithContextHook`].
///
/// Register with [`crate::Agent::set_prepare_next_turn_object`].
#[async_trait]
pub trait PrepareNextTurn: Send + Sync {
    /// Return explicit replacements for later turns in the active run; `None` preserves them.
    async fn prepare(
        &self,
        ctx: PrepareNextTurnContext,
        cancel: CancellationToken,
    ) -> Option<AgentLoopTurnUpdate>;
}

/// Object-safe mirror of [`QueueMessagesHook`], the steering/follow-up message source.
///
/// An empty vector means no messages are queued at that poll. Register a source with
/// [`crate::AgentConfig::with_steering_source_object`] or
/// [`crate::AgentConfig::with_follow_up_source_object`]; see those methods for why the facade exposes
/// this at construction time rather than as a runtime setter.
#[async_trait]
pub trait QueueSource: Send + Sync {
    /// Poll for messages to inject at this steering or follow-up point.
    async fn poll(&self) -> Vec<AgentMessage>;
}

/// Object-safe mirror of [`crate::ConvertToLlm`], the provider-boundary transcript converter.
///
/// The method is synchronous (it mirrors the synchronous [`crate::convert_messages_to_llm`] work the
/// default converter performs); the adapter wraps the returned vector in a ready future to satisfy
/// the async [`crate::ConvertToLlm`] closure alias. Register with
/// [`crate::Agent::set_convert_to_llm_object`].
pub trait MessageConverter: Send + Sync {
    /// Convert the widened transcript snapshot into provider chat messages.
    fn convert(&self, messages: &[AgentMessage]) -> Vec<ChatMessage>;
}
