//! Ordered lifecycle observation for agent runs.
//!
//! The low-level loop owns one mutable [`AgentEventSink`] and awaits every [`AgentEventSink::emit`]
//! call before continuing. This makes sink completion an ordering barrier for later events, hooks,
//! and loop work. The stateful [`crate::Agent`] applies each event to its state before invoking its
//! awaited listeners.

use crate::{AgentMessage, AgentToolResult, AssistantMessageEvent, ToolResultMessage};
use async_trait::async_trait;
use serde_json::Value;

/// High-level lifecycle event emitted by the low-level loop and stateful agent.
///
/// Runtime provider and tool failures are represented in these events as terminal assistant
/// messages or error tool results rather than being returned as loop guard errors.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The invocation has begun, before its first turn starts.
    AgentStart,
    /// The invocation has ended normally, including an in-band error or cancellation ending.
    AgentEnd {
        /// Messages produced during this invocation, excluding its starting transcript.
        messages: Vec<AgentMessage>,
    },
    /// A turn has begun before queued input and the next assistant response are emitted.
    TurnStart,
    /// A turn has ended after its assistant response and tool-call batch settle.
    TurnEnd {
        /// Final assistant message for the turn.
        message: AgentMessage,
        /// Finalized tool-result messages from this turn, in source order.
        tool_results: Vec<ToolResultMessage>,
    },
    /// Emission of one user, custom, assistant, or tool-result message has begun.
    MessageStart {
        /// Current message value at the start boundary.
        message: AgentMessage,
    },
    /// A streaming assistant message has changed.
    MessageUpdate {
        /// Complete partial assistant-message snapshot after this update.
        message: AgentMessage,
        /// Underlying assistant protocol update that produced the snapshot.
        assistant_message_event: AssistantMessageEvent,
    },
    /// A message has reached its final emitted value.
    ///
    /// The stateful facade commits the message to its transcript before notifying listeners of this
    /// event.
    MessageEnd {
        /// Final message value.
        message: AgentMessage,
    },
    /// Execution processing has begun for one requested tool call.
    ToolExecutionStart {
        /// Provider-assigned call identifier.
        tool_call_id: String,
        /// Requested tool name.
        tool_name: String,
        /// Raw arguments from the assistant call before preparation or hook mutation.
        args: Value,
    },
    /// A running tool published an intermediate result.
    ToolExecutionUpdate {
        /// Provider-assigned call identifier.
        tool_call_id: String,
        /// Requested tool name.
        tool_name: String,
        /// Original arguments from the assistant call, before preparation or hook mutation.
        args: Value,
        /// Tool-provided partial result.
        partial_result: AgentToolResult,
    },
    /// Processing has ended for one tool call.
    ToolExecutionEnd {
        /// Provider-assigned call identifier.
        tool_call_id: String,
        /// Requested tool name.
        tool_name: String,
        /// Final result after any after-tool hook overrides.
        result: AgentToolResult,
        /// Final error classification after any after-tool hook override.
        is_error: bool,
    },
}

/// Payload-free discriminator for [`AgentEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Discriminator for [`AgentEvent::AgentStart`].
    AgentStart,
    /// Discriminator for [`AgentEvent::AgentEnd`].
    AgentEnd,
    /// Discriminator for [`AgentEvent::TurnStart`].
    TurnStart,
    /// Discriminator for [`AgentEvent::TurnEnd`].
    TurnEnd,
    /// Discriminator for [`AgentEvent::MessageStart`].
    MessageStart,
    /// Discriminator for [`AgentEvent::MessageUpdate`].
    MessageUpdate,
    /// Discriminator for [`AgentEvent::MessageEnd`].
    MessageEnd,
    /// Discriminator for [`AgentEvent::ToolExecutionStart`].
    ToolExecutionStart,
    /// Discriminator for [`AgentEvent::ToolExecutionUpdate`].
    ToolExecutionUpdate,
    /// Discriminator for [`AgentEvent::ToolExecutionEnd`].
    ToolExecutionEnd,
}

impl AgentEvent {
    /// Return this event's payload-free discriminator.
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::AgentStart => EventKind::AgentStart,
            Self::AgentEnd { .. } => EventKind::AgentEnd,
            Self::TurnStart => EventKind::TurnStart,
            Self::TurnEnd { .. } => EventKind::TurnEnd,
            Self::MessageStart { .. } => EventKind::MessageStart,
            Self::MessageUpdate { .. } => EventKind::MessageUpdate,
            Self::MessageEnd { .. } => EventKind::MessageEnd,
            Self::ToolExecutionStart { .. } => EventKind::ToolExecutionStart,
            Self::ToolExecutionUpdate { .. } => EventKind::ToolExecutionUpdate,
            Self::ToolExecutionEnd { .. } => EventKind::ToolExecutionEnd,
        }
    }
}

/// Awaited event target used as the low-level loop's single writer.
///
/// The loop calls this method through one mutable sink and does not proceed until the returned
/// future completes. Calls therefore arrive in loop emission order; an implementation that spawns
/// detached work is responsible for preserving any ordering it needs beyond that boundary. The
/// signature is infallible, so implementations must not use panics for routine event handling.
#[async_trait]
pub trait AgentEventSink: Send {
    /// Observe one event and complete its ordering barrier.
    async fn emit(&mut self, event: AgentEvent);
}

#[async_trait]
impl<F, Fut> AgentEventSink for F
where
    F: FnMut(AgentEvent) -> Fut + Send,
    Fut: std::future::Future<Output = ()> + Send,
{
    async fn emit(&mut self, event: AgentEvent) {
        (self)(event).await;
    }
}

/// Event sink that immediately discards every event.
#[derive(Debug, Default)]
pub struct NoopEventSink;

#[async_trait]
impl AgentEventSink for NoopEventSink {
    async fn emit(&mut self, _event: AgentEvent) {}
}
