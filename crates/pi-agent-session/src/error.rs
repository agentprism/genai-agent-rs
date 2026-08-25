//! Public storage errors and reducer-integrity failures.

use crate::{EntryId, LaneName, Sequence};
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

/// Stable public session error category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorKind {
    /// Requested session, entry, or repository object was not found.
    NotFound,
    /// A destination or identifier already exists.
    AlreadyExists,
    /// Optimistic append observed a different last sequence.
    SequenceConflict,
    /// A durable mutation violates reducer integrity.
    Corruption,
    /// A lane does not exist or is otherwise invalid.
    InvalidLane,
    /// A query has invalid bounds.
    InvalidQuery,
    /// A branch fork target is invalid.
    InvalidForkTarget,
    /// Backend I/O or synchronization failed.
    Storage,
}

/// Sanitized error returned by session storage and repository boundaries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionError {
    /// Stable error category.
    pub kind: SessionErrorKind,
    /// Human-readable secret-free diagnostic.
    pub message: String,
    /// Expected sequence for optimistic append conflicts.
    pub expected_sequence: Option<Sequence>,
    /// Actual current sequence for optimistic append conflicts.
    pub actual_sequence: Option<Sequence>,
}

impl SessionError {
    /// Creates an error without sequence-conflict metadata.
    pub fn new(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            expected_sequence: None,
            actual_sequence: None,
        }
    }

    /// Creates an optimistic sequence-conflict error.
    pub fn sequence_conflict(expected: Sequence, actual: Sequence) -> Self {
        Self {
            kind: SessionErrorKind::SequenceConflict,
            message: format!(
                "session append expected sequence {expected}, but current sequence is {actual}"
            ),
            expected_sequence: Some(expected),
            actual_sequence: Some(actual),
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SessionError {}

/// Exact reducer invariant violated by a mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionReductionError {
    /// Global sequence was not consecutive.
    SequenceGap {
        /// Required next sequence.
        expected: Sequence,
        /// Mutation sequence.
        actual: Sequence,
    },
    /// Entry or record identifier was already used.
    DuplicateId {
        /// Duplicate identifier text.
        id: String,
    },
    /// Entry parent does not exist earlier in the log.
    MissingParent {
        /// Missing parent.
        parent_id: EntryId,
    },
    /// A lane-bound mutation references a missing lane.
    MissingLane {
        /// Missing lane.
        lane: LaneName,
    },
    /// A lane-bound entry did not chain from the current lane head.
    LaneChainMismatch {
        /// Affected lane.
        lane: LaneName,
        /// Expected current leaf.
        expected_parent: Option<EntryId>,
        /// Entry's actual parent.
        actual_parent: Option<EntryId>,
    },
    /// Lane pointer target does not exist.
    MissingLaneTarget {
        /// Missing target.
        target_id: EntryId,
    },
    /// Label target does not exist.
    MissingLabelTarget {
        /// Missing target.
        target_id: EntryId,
    },
    /// Operational record references an assistant entry that is not committed.
    MissingToolAssistant {
        /// Missing assistant entry.
        assistant_entry_id: EntryId,
    },
    /// Stable tool index does not identify the recorded call.
    ToolIdentityMismatch {
        /// Assistant entry containing the call.
        assistant_entry_id: EntryId,
        /// Stable tool index.
        tool_index: u32,
    },
    /// Two records claim the same assistant tool invocation.
    DuplicateToolInvocation {
        /// Assistant entry containing the call.
        assistant_entry_id: EntryId,
        /// Stable tool index.
        tool_index: u32,
    },
    /// Queue cancellation does not reference an uncommitted enqueue with the same run.
    MissingQueuedEntry {
        /// Unknown queued target.
        entry_id: EntryId,
    },
    /// A statistic or sequence could not be represented.
    ArithmeticOverflow,
}

impl fmt::Display for SessionReductionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceGap { expected, actual } => {
                write!(
                    formatter,
                    "non-consecutive sequence {actual}; expected {expected}"
                )
            }
            Self::DuplicateId { id } => write!(formatter, "duplicate session id {id}"),
            Self::MissingParent { parent_id } => {
                write!(formatter, "entry references missing parent {parent_id}")
            }
            Self::MissingLane { lane } => {
                write!(formatter, "mutation references missing lane {lane}")
            }
            Self::LaneChainMismatch {
                lane,
                expected_parent,
                actual_parent,
            } => write!(
                formatter,
                "entry on lane {lane} has parent {actual_parent:?}; expected {expected_parent:?}"
            ),
            Self::MissingLaneTarget { target_id } => {
                write!(formatter, "lane references missing entry {target_id}")
            }
            Self::MissingLabelTarget { target_id } => {
                write!(formatter, "label references missing entry {target_id}")
            }
            Self::MissingToolAssistant { assistant_entry_id } => write!(
                formatter,
                "tool record references unknown assistant entry {assistant_entry_id}"
            ),
            Self::ToolIdentityMismatch {
                assistant_entry_id,
                tool_index,
            } => write!(
                formatter,
                "tool record does not match assistant {assistant_entry_id} tool index {tool_index}"
            ),
            Self::DuplicateToolInvocation {
                assistant_entry_id,
                tool_index,
            } => write!(
                formatter,
                "assistant {assistant_entry_id} tool index {tool_index} was already started"
            ),
            Self::MissingQueuedEntry { entry_id } => {
                write!(
                    formatter,
                    "queue cancellation has no uncommitted matching enqueue for {entry_id}"
                )
            }
            Self::ArithmeticOverflow => formatter.write_str("session arithmetic overflow"),
        }
    }
}

impl Error for SessionReductionError {}

impl From<SessionReductionError> for SessionError {
    fn from(error: SessionReductionError) -> Self {
        Self::new(SessionErrorKind::Corruption, error.to_string())
    }
}
