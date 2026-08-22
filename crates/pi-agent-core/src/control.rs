//! Concurrent queue ingress and run cancellation from Architecture v2 part 2
//! §8.4.

use crate::AgentRecord;
use pi_ai::{CancellationToken, RunId};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

/// Default maximum number of in-memory commands accepted by a bare agent.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1_024;

/// Monotonic ingress order shared by steering and follow-up producers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueueSequence(
    /// One-based command sequence.
    pub u64,
);

/// The two semantically distinct agent queues.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    /// Inject after a completed turn and before automatic continuation.
    Steering,
    /// Inject only when the agent would otherwise stop.
    FollowUp,
}

/// Queue drain behavior, stored independently for each queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueDrainMode {
    /// Drain the oldest command only.
    #[default]
    One,
    /// Drain every currently queued command in ingress order.
    All,
}

/// One accepted queue command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueCommand {
    /// Globally monotonic queue ingress sequence.
    pub sequence: QueueSequence,
    /// Destination queue.
    pub kind: QueueKind,
    /// Durable message to inject at the queue's defined phase boundary.
    pub message: AgentRecord,
}

/// Acknowledgement that the bounded in-memory queue accepted a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueueReceipt {
    /// Accepted ingress sequence.
    pub sequence: QueueSequence,
    /// Queue that accepted the command.
    pub kind: QueueKind,
}

/// Failure to apply a concurrent agent-control command.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlError {
    /// The agent owning this handle has been dropped.
    Closed,
    /// The bounded in-memory queue is full.
    QueueFull {
        /// Configured total capacity across both queues.
        capacity: usize,
    },
    /// No active run has the supplied identity.
    UnknownRun {
        /// Run the caller attempted to cancel.
        run_id: RunId,
    },
    /// The queue sequence cannot be incremented.
    SequenceOverflow,
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("agent control handle is closed"),
            Self::QueueFull { capacity } => {
                write!(formatter, "agent queue capacity {capacity} is exhausted")
            }
            Self::UnknownRun { run_id } => write!(formatter, "run {run_id} is not active"),
            Self::SequenceOverflow => formatter.write_str("queue command sequence overflowed"),
        }
    }
}

impl std::error::Error for ControlError {}

#[derive(Clone)]
pub(crate) struct QueueReceiver {
    shared: Arc<ControlShared>,
}

/// Cloneable concurrent ingress and cancellation capability.
#[derive(Clone)]
pub struct AgentControl {
    shared: Arc<ControlShared>,
}

struct ControlShared {
    state: Mutex<ControlState>,
    active: Mutex<Option<ActiveRun>>,
}

struct ActiveRun {
    run_id: RunId,
    cancellation: CancellationToken,
}

struct ControlState {
    closed: bool,
    capacity: usize,
    next_sequence: u64,
    steering_mode: QueueDrainMode,
    follow_up_mode: QueueDrainMode,
    steering: VecDeque<QueueCommand>,
    follow_up: VecDeque<QueueCommand>,
}

impl AgentControl {
    pub(crate) fn channel(capacity: usize) -> (Self, QueueReceiver) {
        let shared = Arc::new(ControlShared {
            state: Mutex::new(ControlState {
                closed: false,
                capacity,
                next_sequence: 1,
                steering_mode: QueueDrainMode::One,
                follow_up_mode: QueueDrainMode::One,
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
            }),
            active: Mutex::new(None),
        });
        (
            Self {
                shared: shared.clone(),
            },
            QueueReceiver { shared },
        )
    }

    /// Enqueues one steering record and resolves after the bounded queue accepts it.
    pub async fn steer(&self, message: AgentRecord) -> Result<QueueReceipt, ControlError> {
        self.enqueue(QueueKind::Steering, message)
    }

    /// Enqueues one follow-up record and resolves after the bounded queue accepts it.
    pub async fn follow_up(&self, message: AgentRecord) -> Result<QueueReceipt, ControlError> {
        self.enqueue(QueueKind::FollowUp, message)
    }

    /// Cancels the active run only when its stable identity matches.
    pub fn cancel(&self, run_id: RunId) -> Result<(), ControlError> {
        let cancellation = {
            let active = lock_unpoisoned(&self.shared.active);
            let Some(active) = active.as_ref() else {
                return Err(ControlError::UnknownRun { run_id });
            };
            if active.run_id != run_id {
                return Err(ControlError::UnknownRun { run_id });
            }
            active.cancellation.clone()
        };
        cancellation.cancel();
        Ok(())
    }

    /// Removes every queued steering command and returns the number removed.
    pub fn clear_steering(&self) -> usize {
        let mut state = lock_unpoisoned(&self.shared.state);
        let count = state.steering.len();
        state.steering.clear();
        count
    }

    /// Removes every queued follow-up command and returns the number removed.
    pub fn clear_follow_up(&self) -> usize {
        let mut state = lock_unpoisoned(&self.shared.state);
        let count = state.follow_up.len();
        state.follow_up.clear();
        count
    }

    /// Removes both queues and returns the total number of commands removed.
    pub fn clear_all(&self) -> usize {
        let mut state = lock_unpoisoned(&self.shared.state);
        let count = state.steering.len() + state.follow_up.len();
        state.steering.clear();
        state.follow_up.clear();
        count
    }

    /// Sets steering drain behavior without changing queued commands.
    pub fn set_steering_mode(&self, mode: QueueDrainMode) {
        lock_unpoisoned(&self.shared.state).steering_mode = mode;
    }

    /// Returns steering drain behavior.
    pub fn steering_mode(&self) -> QueueDrainMode {
        lock_unpoisoned(&self.shared.state).steering_mode
    }

    /// Sets follow-up drain behavior without changing queued commands.
    pub fn set_follow_up_mode(&self, mode: QueueDrainMode) {
        lock_unpoisoned(&self.shared.state).follow_up_mode = mode;
    }

    /// Returns follow-up drain behavior.
    pub fn follow_up_mode(&self) -> QueueDrainMode {
        lock_unpoisoned(&self.shared.state).follow_up_mode
    }

    fn enqueue(&self, kind: QueueKind, message: AgentRecord) -> Result<QueueReceipt, ControlError> {
        let mut state = lock_unpoisoned(&self.shared.state);
        if state.closed {
            return Err(ControlError::Closed);
        }
        if state.steering.len() + state.follow_up.len() >= state.capacity {
            return Err(ControlError::QueueFull {
                capacity: state.capacity,
            });
        }
        let sequence = QueueSequence(state.next_sequence);
        state.next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(ControlError::SequenceOverflow)?;
        let command = QueueCommand {
            sequence,
            kind,
            message,
        };
        match kind {
            QueueKind::Steering => state.steering.push_back(command),
            QueueKind::FollowUp => state.follow_up.push_back(command),
        }
        Ok(QueueReceipt { sequence, kind })
    }
}

impl fmt::Debug for AgentControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock_unpoisoned(&self.shared.state);
        formatter
            .debug_struct("AgentControl")
            .field("closed", &state.closed)
            .field("steering_len", &state.steering.len())
            .field("follow_up_len", &state.follow_up.len())
            .finish_non_exhaustive()
    }
}

impl QueueReceiver {
    pub(crate) fn register_run(
        &self,
        run_id: RunId,
        cancellation: CancellationToken,
    ) -> Result<(), ControlError> {
        if lock_unpoisoned(&self.shared.state).closed {
            return Err(ControlError::Closed);
        }
        let mut active = lock_unpoisoned(&self.shared.active);
        if active.is_some() {
            return Err(ControlError::UnknownRun { run_id });
        }
        *active = Some(ActiveRun {
            run_id,
            cancellation,
        });
        Ok(())
    }

    pub(crate) fn unregister_run(&self, run_id: &RunId) {
        let mut active = lock_unpoisoned(&self.shared.active);
        if active
            .as_ref()
            .is_some_and(|active| &active.run_id == run_id)
        {
            *active = None;
        }
    }

    pub(crate) fn drain(&self, kind: QueueKind) -> Vec<QueueCommand> {
        let mut state = lock_unpoisoned(&self.shared.state);
        let (mode, queue) = match kind {
            QueueKind::Steering => (state.steering_mode, &mut state.steering),
            QueueKind::FollowUp => (state.follow_up_mode, &mut state.follow_up),
        };
        match mode {
            QueueDrainMode::One => queue.pop_front().into_iter().collect(),
            QueueDrainMode::All => queue.drain(..).collect(),
        }
    }

    pub(crate) fn drain_continue_tail(&self) -> (Vec<QueueCommand>, Vec<QueueCommand>) {
        let mut state = lock_unpoisoned(&self.shared.state);
        let steering_mode = state.steering_mode;
        let steering = drain_queue(&mut state.steering, steering_mode);
        if !steering.is_empty() {
            return (steering, Vec::new());
        }
        let follow_up_mode = state.follow_up_mode;
        let follow_up = drain_queue(&mut state.follow_up, follow_up_mode);
        (Vec::new(), follow_up)
    }

    pub(crate) fn clear_all(&self) {
        let mut state = lock_unpoisoned(&self.shared.state);
        state.steering.clear();
        state.follow_up.clear();
    }

    pub(crate) fn close(&self) {
        {
            let mut state = lock_unpoisoned(&self.shared.state);
            state.closed = true;
            state.steering.clear();
            state.follow_up.clear();
        }
        if let Some(active) = lock_unpoisoned(&self.shared.active).take() {
            active.cancellation.cancel();
        }
    }
}

fn drain_queue(queue: &mut VecDeque<QueueCommand>, mode: QueueDrainMode) -> Vec<QueueCommand> {
    match mode {
        QueueDrainMode::One => queue.pop_front().into_iter().collect(),
        QueueDrainMode::All => queue.drain(..).collect(),
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
