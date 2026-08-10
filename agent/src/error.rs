//! Guard and protocol errors exposed by the agent APIs.
//!
//! Low-level [`LoopError`] and stateful [`AgentError`] values report invalid invocations or missing
//! runtime dependencies. Ordinary provider failures, cancellation, and tool failures are encoded
//! in-band as assistant messages, tool-result messages, and lifecycle events. Legacy hook
//! signatures are likewise infallible; a hook panic is a contract violation, not a routine error
//! value. The opt-in fallible tool channels ([`ToolHookError`]) keep the same in-band rule: their
//! errors become error tool results rather than loop errors.

use thiserror::Error;

/// Errors raised by the low-level loop for guards, missing runtime pieces, or a spawned-task
/// contract violation.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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

/// Errors produced by the fallible tool channels: [`crate::AgentTool::try_prepare_arguments`],
/// [`crate::TryBeforeToolCallHook`], and [`crate::TryAfterToolCallHook`].
///
/// The loop converts these errors into ordinary in-band error tool results instead of returning
/// a [`LoopError`] or aborting the run: preparation and before-hook failures skip execution,
/// while an after-hook failure replaces the completed result. [`std::fmt::Display`] is exactly
/// the contained message — the in-band result text — mirroring pi's `error.message` propagation
/// for a thrown `prepareArguments`/`beforeToolCall`/`afterToolCall`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ToolHookError {
    message: String,
}

impl ToolHookError {
    /// Construct a channel error whose display text is the supplied message, verbatim.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Return the exact message used as the in-band error tool-result text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<String> for ToolHookError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ToolHookError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
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

// `StreamProtocolError` was relocated into the `genai` fork crate alongside the assistant
// event-stream adapters that produce it. Re-exported here so this module's public path
// (`crate::error::StreamProtocolError`) and its crate-root re-export stay unchanged.
pub use genai::assistant_stream::StreamProtocolError;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_hook_error_display_is_the_verbatim_message() {
        let error = ToolHookError::new("hook exploded");
        assert_eq!(error.to_string(), "hook exploded");
        assert_eq!(error.message(), "hook exploded");
        let from_string: ToolHookError = String::from("owned").into();
        assert_eq!(from_string.to_string(), "owned");
        let from_str: ToolHookError = "borrowed".into();
        assert_eq!(from_str.to_string(), "borrowed");
    }
}
