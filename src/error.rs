//! Guard and protocol errors exposed by the agent APIs.
//!
//! Low-level [`LoopError`] and stateful [`AgentError`] values report invalid invocations or missing
//! runtime dependencies. Ordinary provider failures, cancellation, and tool failures are encoded
//! in-band as assistant messages, tool-result messages, and lifecycle events. Hook signatures are
//! likewise infallible; a hook panic is a contract violation, not a routine error value.

use thiserror::Error;

/// Errors raised by the low-level loop for guards, missing runtime pieces, or a spawned-task
/// contract violation.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum LoopError {
    /// A continuation was requested with an empty transcript.
    #[error("Cannot continue: no messages in context")]
    EmptyContext,
    /// A continuation was requested after a message whose declared role is `"assistant"`.
    #[error("Cannot continue from message role: assistant")]
    ContinueFromAssistant,
    /// Neither the invocation nor the process supplied a provider stream function.
    #[error(
        "No default stream function configured. Pass stream_fn explicitly or call set_default_stream_fn()."
    )]
    NoDefaultStreamFn,
    /// A task spawned by the event-stream convenience API panicked or ended without publishing.
    ///
    /// The string is the recovered panic payload or a defensive diagnostic.
    #[error("agent loop task panicked: {0}")]
    TaskPanicked(String),
}

/// The admission path that rejected a call because another run was active.
///
/// Carried by [`AgentError::Busy`], this selects the exact message text; each string matches the
/// corresponding site-specific TypeScript error byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyContext {
    /// A new prompt was rejected while a run was active.
    Prompt,
    /// A continuation was rejected while a run was active.
    Continue,
    /// A reset was rejected while a run was active.
    Reset,
    /// A guarded operation such as a runtime-configuration setter was rejected while a run was
    /// active.
    Other,
}

impl std::fmt::Display for BusyContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Prompt => {
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
            }
            Self::Continue => "Agent is already processing. Wait for completion before continuing.",
            Self::Reset => "Agent is already processing. Wait for completion before resetting.",
            Self::Other => "Agent is already processing.",
        })
    }
}

/// Admission and continuation errors returned by the stateful [`crate::Agent`] facade.
///
/// A successful admission returns `Ok(())` even when the run later ends with an in-band provider,
/// tool, cancellation, or recovered loop failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentError {
    /// Another prompt or continuation is active.
    ///
    /// The payload records which admission path rejected the call and selects the message text.
    /// Guarded runtime-configuration setters return [`BusyContext::Other`] while a run is active.
    #[error("{0}")]
    Busy(BusyContext),
    /// [`crate::Agent::continue_`] was called without any transcript messages.
    #[error("No messages to continue from")]
    EmptyContext,
    /// Continuation ended at an assistant message and neither queue could provide input.
    #[error("Cannot continue from message role: assistant")]
    ContinueFromAssistant,
    /// Neither this agent nor the process has a provider stream function installed.
    #[error(
        "No default stream function configured. Pass stream_fn explicitly or call set_default_stream_fn()."
    )]
    NoDefaultStreamFn,
}

/// Errors produced by an [`crate::AgentTool`] implementation.
///
/// The loop converts these errors to in-band error tool results instead of returning a
/// [`LoopError`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolError {
    /// Tool execution observed cancellation and stopped.
    #[error("tool execution was cancelled")]
    Cancelled,
    /// The tool rejected its prepared arguments; the string explains the rejection.
    #[error("tool arguments are invalid: {0}")]
    InvalidArguments(String),
    /// The tool failed during execution; the string explains the failure.
    #[error("tool execution failed: {0}")]
    Execution(String),
}

impl ToolError {
    /// Construct an execution failure from any string-like diagnostic.
    pub fn execution(error: impl Into<String>) -> Self {
        Self::Execution(error.into())
    }
}

/// A malformed assistant event stream (for example, one that closes without a terminal event).
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum StreamProtocolError {
    /// The stream closed before publishing a terminal `Done` or `Error` event.
    #[error("assistant event stream closed without a terminal Done or Error event")]
    MissingTerminalEvent,
}

/// Errors produced while validating tool arguments.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// Arguments failed the named tool's JSON-schema validation.
    #[error("Validation failed for tool \"{tool_name}\":{message}")]
    Invalid {
        /// Name of the tool whose arguments were checked.
        tool_name: String,
        /// Human-readable validation diagnostic.
        message: String,
    },
}
