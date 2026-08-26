//! Harness configuration and invariant errors from Architecture v2 part 1 §7.

use agentprism_ai::{MessageId, ModelRef, RunId};
use std::fmt;

/// Failure caused by invalid agent configuration, incompatible persisted
/// state, or a violated state-machine invariant.
///
/// Provider failures, cancellation, and tool failures are deliberately absent:
/// those are expected operational outcomes represented in transcript records
/// and [`crate::RunOutcome`].
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentError {
    /// The persisted snapshot schema cannot be migrated by this crate version.
    UnsupportedSnapshotSchema {
        /// Schema found in persistence.
        found: u32,
        /// Newest schema understood by this crate version.
        supported: u32,
    },
    /// The embedded durable-state schema cannot be migrated.
    UnsupportedStateSchema {
        /// Schema found in persistence.
        found: u32,
        /// Newest schema understood by this crate version.
        supported: u32,
    },
    /// A snapshot cannot allocate a valid next event sequence.
    InvalidNextSequence {
        /// Invalid persisted sequence.
        next_sequence: u64,
    },
    /// The current model catalog cannot resolve the persisted model reference.
    UnresolvedModel {
        /// Missing provider/model identity.
        model: ModelRef,
    },
    /// The application did not register a persisted custom record kind.
    UnknownCustomRecordKind {
        /// Unresolved custom kind.
        type_name: String,
    },
    /// A tool registry contains an empty tool name.
    InvalidToolName,
    /// A tool registry attempted to bind the same name twice.
    DuplicateToolName {
        /// Duplicate model-facing tool name.
        name: String,
    },
    /// A model-facing tool specification contains an invalid JSON Schema.
    InvalidToolSchema {
        /// Tool whose argument schema could not be compiled.
        name: String,
        /// Sanitized schema compiler diagnostic.
        message: String,
    },
    /// An operation requiring an idle agent was requested during a run.
    RunActive,
    /// Continue was requested without any durable transcript records.
    ContinueWithoutMessages,
    /// Continue was requested while an assistant record remained at the tail.
    ContinueFromAssistant,
    /// Retry requires an errored or aborted assistant at the durable tail.
    RetryRequiresFailedAssistant,
    /// Event envelopes were not replayed in consecutive sequence order.
    EventSequenceMismatch {
        /// Required next sequence.
        expected: u64,
        /// Sequence carried by the envelope.
        actual: u64,
    },
    /// Incrementing an event sequence overflowed `u64`.
    EventSequenceOverflow,
    /// An event's embedded run identity disagreed with its envelope.
    EventRunIdMismatch {
        /// Run identity from the envelope.
        envelope: RunId,
        /// Run identity embedded in the event.
        event: RunId,
    },
    /// A committed LLM message reused an existing durable message identifier.
    DuplicateMessageId {
        /// Reused canonical identifier.
        message_id: MessageId,
    },
    /// A configuration value not covered by a more precise variant is invalid.
    InvalidConfiguration {
        /// Sanitized configuration diagnostic.
        message: String,
    },
    /// The state machine reached a state forbidden by its protocol.
    InvariantViolation {
        /// Sanitized invariant diagnostic.
        message: String,
    },
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSnapshotSchema { found, supported } => write!(
                formatter,
                "unsupported agent snapshot schema {found}; newest supported schema is {supported}"
            ),
            Self::UnsupportedStateSchema { found, supported } => write!(
                formatter,
                "unsupported agent state schema {found}; newest supported schema is {supported}"
            ),
            Self::InvalidNextSequence { next_sequence } => {
                write!(
                    formatter,
                    "invalid next agent event sequence {next_sequence}"
                )
            }
            Self::UnresolvedModel { model } => write!(
                formatter,
                "cannot resolve persisted model {}/{}",
                model.provider, model.model
            ),
            Self::UnknownCustomRecordKind { type_name } => {
                write!(
                    formatter,
                    "unregistered custom agent record kind {type_name}"
                )
            }
            Self::InvalidToolName => formatter.write_str("tool name must not be empty"),
            Self::DuplicateToolName { name } => {
                write!(formatter, "tool {name} is registered more than once")
            }
            Self::InvalidToolSchema { name, message } => {
                write!(
                    formatter,
                    "invalid argument schema for tool {name}: {message}"
                )
            }
            Self::RunActive => formatter.write_str("agent is already processing a run"),
            Self::ContinueWithoutMessages => {
                formatter.write_str("cannot continue: no messages in transcript")
            }
            Self::ContinueFromAssistant => {
                formatter.write_str("cannot continue from an assistant record")
            }
            Self::RetryRequiresFailedAssistant => formatter
                .write_str("retry requires an error or aborted assistant at the transcript tail"),
            Self::EventSequenceMismatch { expected, actual } => write!(
                formatter,
                "agent event sequence mismatch: expected {expected}, received {actual}"
            ),
            Self::EventSequenceOverflow => formatter.write_str("agent event sequence overflowed"),
            Self::EventRunIdMismatch { envelope, event } => write!(
                formatter,
                "agent event run id {event} does not match envelope run id {envelope}"
            ),
            Self::DuplicateMessageId { message_id } => {
                write!(
                    formatter,
                    "message id {message_id} was committed more than once"
                )
            }
            Self::InvalidConfiguration { message } => formatter.write_str(message),
            Self::InvariantViolation { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AgentError {}
