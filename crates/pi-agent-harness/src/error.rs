//! Harness policy and orchestration errors.

use pi_agent_core::{AgentError, ControlError};
use pi_agent_session::{EntryId, LaneName, OperationRecord, SessionError};
use pi_ai::RequestStartError;
use std::fmt;

/// Machine-readable contradiction in a lane's durable recovery prefix.
///
/// These names mirror pinned Pi's `RecordLogCorruptionReason` values for the
/// live orchestration invariants implemented by this package.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryCorruptionReason {
    /// A non-start record names an operation that was never started.
    UnknownOperation,
    /// An operation-owned record follows that operation's finish record.
    RecordAfterFinish,
    /// Attempts in one unfinished logical step do not increment by one.
    NonConsecutiveAttempt,
    /// A compaction reason is missing or attached to the wrong step kind.
    InvalidCompactionReason,
    /// Steering or follow-up ingress follows a durable abort request.
    QueueAfterAbort,
    /// A cancellation does not refer to a matching pending queue record.
    InvalidQueueCancellation,
    /// Structural attempts disagree about their stable result intent.
    InconsistentStep,
    /// A durable tool result contradicts its recorded assistant call.
    ToolCallMismatch,
    /// Two durable starts claim the same assistant/tool ordinal.
    DuplicateToolInvocation,
    /// A materialized provisioned entry differs from its recorded intent.
    ProvisionedEntryMismatch,
    /// A deferred assistant terminal is missing its provider handle.
    InvalidDeferredHandle,
}

/// Failure at the durable harness orchestration boundary.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessError {
    /// Durable session state could not be read or advanced.
    Session(SessionError),
    /// The core agent rejected an operation or violated an invariant.
    Agent(AgentError),
    /// Durable branch context could not be reconstructed.
    Context {
        /// Sanitized reconstruction diagnostic.
        message: String,
    },
    /// Concurrent queue ingress or cancellation failed.
    Control(ControlError),
    /// More than one unresolved operation was recovered on one lane.
    CorruptOpenOperations {
        /// Conflicting operation starts, newest first.
        open_operations: Vec<OperationRecord>,
    },
    /// The durable record prefix contradicts pinned Pi's reducer protocol.
    CorruptRecordLog {
        /// Stable contradiction category.
        reason: RecoveryCorruptionReason,
        /// Sanitized reducer diagnostic.
        message: String,
    },
    /// A queue requiring an active run was used while the harness was idle.
    NoActiveRun {
        /// Selected durable lane.
        lane: LaneName,
    },
    /// The active durable operation changed before a queue record committed.
    OperationChanged {
        /// Operation observed before durable append.
        expected: pi_ai::RunId,
    },
    /// The core run ended without committing any durable branch entry.
    MissingRunLeaf {
        /// Durable operation identity.
        run_id: pi_ai::RunId,
    },
    /// The open recovery record is not an operation start.
    InvalidRecoveryRecord,
    /// Recovery is valid but belongs to a different harness operation kind.
    UnsupportedRecovery {
        /// Durable operation kind.
        operation: String,
    },
    /// The nested core stream ended without its required terminal event.
    IncompleteAgentStream,
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Agent(error) => error.fmt(formatter),
            Self::Context { message } => formatter.write_str(message),
            Self::Control(error) => error.fmt(formatter),
            Self::CorruptOpenOperations { open_operations } => write!(
                formatter,
                "durable lane has {} unresolved operations",
                open_operations.len()
            ),
            Self::CorruptRecordLog { reason, message } => {
                write!(
                    formatter,
                    "durable recovery log is corrupt ({reason:?}): {message}"
                )
            }
            Self::NoActiveRun { lane } => write!(formatter, "lane {lane} has no active run"),
            Self::OperationChanged { expected } => {
                write!(
                    formatter,
                    "active durable operation changed from {expected}"
                )
            }
            Self::MissingRunLeaf { run_id } => {
                write!(
                    formatter,
                    "run {run_id} finished without a durable branch leaf"
                )
            }
            Self::InvalidRecoveryRecord => {
                formatter.write_str("recovery decision did not contain an operation-start record")
            }
            Self::UnsupportedRecovery { operation } => {
                write!(
                    formatter,
                    "cannot resume {operation} through the run operation surface"
                )
            }
            Self::IncompleteAgentStream => {
                formatter.write_str("agent stream ended without RunFinished")
            }
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<SessionError> for HarnessError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<AgentError> for HarnessError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<ControlError> for HarnessError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

/// Failure while deciding, generating, or durably committing a compaction.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionError {
    /// The operation observed cancellation.
    Cancelled,
    /// The policy could not make a valid compaction decision.
    Decision {
        /// Sanitized policy diagnostic.
        message: String,
    },
    /// The summary model failed or returned unusable output.
    Summarization {
        /// Sanitized model diagnostic.
        message: String,
    },
    /// Durable session state could not be read or advanced.
    Session {
        /// Sanitized storage or reducer diagnostic.
        message: String,
    },
    /// The durable operation cannot be resumed as a compaction.
    NotResumable {
        /// Sanitized recovery diagnostic.
        message: String,
    },
}

impl CompactionError {
    /// Creates a policy-decision failure.
    pub fn decision(message: impl Into<String>) -> Self {
        Self::Decision {
            message: message.into(),
        }
    }

    /// Creates a summary-generation failure.
    pub fn summarization(message: impl Into<String>) -> Self {
        Self::Summarization {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("compaction was cancelled"),
            Self::Decision { message }
            | Self::Summarization { message }
            | Self::Session { message }
            | Self::NotResumable { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CompactionError {}

impl From<SessionError> for CompactionError {
    fn from(error: SessionError) -> Self {
        Self::Session {
            message: error.to_string(),
        }
    }
}

impl From<RequestStartError> for CompactionError {
    fn from(error: RequestStartError) -> Self {
        Self::Summarization {
            message: error.to_string(),
        }
    }
}

/// Failure while collecting, generating, or committing a branch summary.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchSummaryError {
    /// The operation observed cancellation.
    Cancelled,
    /// The target entry does not exist.
    InvalidTarget {
        /// Missing target.
        target_id: EntryId,
    },
    /// The selected lane does not exist.
    InvalidLane {
        /// Missing lane.
        lane: LaneName,
    },
    /// Summary generation failed.
    Summarization {
        /// Sanitized model diagnostic.
        message: String,
    },
    /// Durable session state could not be read or advanced.
    Session {
        /// Sanitized storage or reducer diagnostic.
        message: String,
    },
    /// The durable navigation operation cannot be resumed.
    NotResumable {
        /// Sanitized recovery diagnostic.
        message: String,
    },
}

impl BranchSummaryError {
    /// Creates a summary-generation failure.
    pub fn summarization(message: impl Into<String>) -> Self {
        Self::Summarization {
            message: message.into(),
        }
    }
}

impl fmt::Display for BranchSummaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("branch summarization was cancelled"),
            Self::InvalidTarget { target_id } => write!(formatter, "entry {target_id} not found"),
            Self::InvalidLane { lane } => write!(formatter, "lane {lane} not found"),
            Self::Summarization { message }
            | Self::Session { message }
            | Self::NotResumable { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for BranchSummaryError {}

impl From<SessionError> for BranchSummaryError {
    fn from(error: SessionError) -> Self {
        Self::Session {
            message: error.to_string(),
        }
    }
}

impl From<RequestStartError> for BranchSummaryError {
    fn from(error: RequestStartError) -> Self {
        Self::Summarization {
            message: error.to_string(),
        }
    }
}

/// Failure from harness-level assistant overflow recovery.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessOperationError {
    /// Context preparation or compaction failed.
    Compaction(CompactionError),
    /// The caller-supplied assistant step failed before producing a terminal message.
    AssistantStep {
        /// Sanitized step diagnostic.
        message: String,
    },
    /// The same operation already consumed its single overflow recovery.
    OverflowRecoveryExhausted,
}

impl fmt::Display for HarnessOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compaction(error) => error.fmt(formatter),
            Self::AssistantStep { message } => formatter.write_str(message),
            Self::OverflowRecoveryExhausted => {
                formatter.write_str("context overflow recovery was already used by this operation")
            }
        }
    }
}

impl std::error::Error for HarnessOperationError {}

impl From<CompactionError> for HarnessOperationError {
    fn from(error: CompactionError) -> Self {
        Self::Compaction(error)
    }
}
