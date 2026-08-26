//! Harness policy and orchestration errors.

use pi_agent_session::{EntryId, LaneName, SessionError};
use pi_ai::RequestStartError;
use std::fmt;

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
