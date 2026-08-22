//! Deterministic reduction of committed event envelopes into durable agent
//! state (Architecture v2 part 1 §8).

use crate::{
    AGENT_INITIAL_SEQUENCE, AgentError, AgentEvent, AgentEventEnvelope, AgentRecord, AgentState,
};
use pi_ai::MessageId;
use std::collections::BTreeSet;

/// Incremental reducer for the durable portion of agent events.
///
/// Every envelope advances the sequence, while only
/// [`AgentEvent::MessageCommitted`] mutates durable transcript state. This
/// mirrors Pi's high-level reducer, which updates state before notifying
/// subscribers, without persisting partial-message scratch.
pub struct CommittedEventReplay {
    state: AgentState,
    next_sequence: u64,
    message_ids: BTreeSet<MessageId>,
}

impl CommittedEventReplay {
    /// Creates a reducer at an explicit next sequence.
    pub fn new(state: AgentState, next_sequence: u64) -> Result<Self, AgentError> {
        if next_sequence < AGENT_INITIAL_SEQUENCE {
            return Err(AgentError::InvalidNextSequence { next_sequence });
        }

        let mut message_ids = BTreeSet::new();
        for record in &state.transcript {
            if let Some(message_id) = record.message_id()
                && !message_ids.insert(message_id.clone())
            {
                return Err(AgentError::DuplicateMessageId {
                    message_id: message_id.clone(),
                });
            }
        }

        Ok(Self {
            state,
            next_sequence,
            message_ids,
        })
    }

    /// Applies one consecutive envelope.
    pub fn apply(&mut self, envelope: &AgentEventEnvelope) -> Result<(), AgentError> {
        if envelope.sequence != self.next_sequence {
            return Err(AgentError::EventSequenceMismatch {
                expected: self.next_sequence,
                actual: envelope.sequence,
            });
        }

        match &envelope.event {
            AgentEvent::RunStarted { run_id } | AgentEvent::TurnStarted { run_id, .. }
                if run_id != &envelope.run_id =>
            {
                return Err(AgentError::EventRunIdMismatch {
                    envelope: envelope.run_id.clone(),
                    event: run_id.clone(),
                });
            }
            AgentEvent::MessageCommitted { message } => {
                if let Some(message_id) = message.message_id()
                    && !self.message_ids.insert(message_id.clone())
                {
                    return Err(AgentError::DuplicateMessageId {
                        message_id: message_id.clone(),
                    });
                }
                self.state.transcript.push(message.clone());
            }
            AgentEvent::RunStarted { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::ContextPrepared { .. }
            | AgentEvent::MessageStarted { .. }
            | AgentEvent::AssistantUpdate { .. }
            | AgentEvent::ToolExecutionStarted { .. }
            | AgentEvent::ToolExecutionUpdated { .. }
            | AgentEvent::ToolExecutionFinished { .. }
            | AgentEvent::TurnFinished { .. }
            | AgentEvent::RunFinished { .. } => {}
        }

        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(AgentError::EventSequenceOverflow)?;
        Ok(())
    }

    /// Returns the current reduced durable state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Returns the sequence required by the next envelope.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Consumes the reducer and returns durable state.
    pub fn into_state(self) -> AgentState {
        self.state
    }
}

/// Replays a complete event history beginning at sequence one and returns the
/// resulting durable state.
pub fn replay_committed_events<'a>(
    initial_state: AgentState,
    events: impl IntoIterator<Item = &'a AgentEventEnvelope>,
) -> Result<AgentState, AgentError> {
    let mut replay = CommittedEventReplay::new(initial_state, AGENT_INITIAL_SEQUENCE)?;
    for event in events {
        replay.apply(event)?;
    }
    Ok(replay.into_state())
}

/// Returns a committed record clone when an event mutates durable transcript
/// state.
pub fn committed_record(event: &AgentEvent) -> Option<AgentRecord> {
    match event {
        AgentEvent::MessageCommitted { message } => Some(message.clone()),
        _ => None,
    }
}
