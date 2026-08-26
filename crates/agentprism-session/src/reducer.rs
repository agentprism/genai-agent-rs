//! Authoritative session-log reducer and recovery projection.

use crate::{
    EntryId, LaneName, OperationRecord, RecoveryDecision, SESSION_STATE_SCHEMA_VERSION, Sequence,
    SessionEntry, SessionFact, SessionMutation, SessionReductionError, SessionStats,
};
use agentprism_ai::{ContentBlock, Message, PublicError};
use agentprism_core::AgentRecord;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Reducer contract from Architecture v2 part 2 §7.4.
pub trait SessionReducer {
    /// Applies one authoritative mutation while enforcing reducer integrity.
    fn apply(&mut self, mutation: &SessionMutation) -> Result<(), SessionReductionError>;

    /// Returns the complete state derived so far.
    fn state(&self) -> &SessionState;
}

/// Complete in-memory projection derived from the authoritative append log.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionState {
    /// Schema of this derived state representation.
    pub schema_version: u32,
    sequence: Sequence,
    entries: BTreeMap<EntryId, SessionEntry>,
    entry_order: Vec<EntryId>,
    records: Vec<OperationRecord>,
    lanes: BTreeMap<LaneName, Option<EntryId>>,
    lane_order: Vec<LaneName>,
    log: Vec<SessionMutation>,
    used_ids: BTreeSet<String>,
    open_operations: BTreeMap<LaneName, Vec<OperationRecord>>,
    queue_enqueue_history: BTreeMap<(LaneName, EntryId), PendingQueueEntry>,
    pending_queue_entries: BTreeMap<(LaneName, EntryId), PendingQueueEntry>,
    cancelled_queue_entries: BTreeSet<(LaneName, EntryId)>,
    tool_invocations: BTreeSet<(LaneName, EntryId, u32)>,
    name: Option<String>,
    labels: BTreeMap<EntryId, String>,
    stats: SessionStats,
}

impl Default for SessionState {
    fn default() -> Self {
        let mut lanes = BTreeMap::new();
        lanes.insert(LaneName::new("main"), None);
        Self {
            schema_version: SESSION_STATE_SCHEMA_VERSION,
            sequence: Sequence::ZERO,
            entries: BTreeMap::new(),
            entry_order: Vec::new(),
            records: Vec::new(),
            lanes,
            lane_order: vec![LaneName::new("main")],
            log: Vec::new(),
            used_ids: BTreeSet::new(),
            open_operations: BTreeMap::new(),
            queue_enqueue_history: BTreeMap::new(),
            pending_queue_entries: BTreeMap::new(),
            cancelled_queue_entries: BTreeSet::new(),
            tool_invocations: BTreeSet::new(),
            name: None,
            labels: BTreeMap::new(),
            stats: SessionStats::default(),
        }
    }
}

impl SessionState {
    /// Creates an empty state with the permanent `main` lane at the root.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replays a complete mutation sequence into a new state.
    pub fn replay(
        mutations: impl IntoIterator<Item = SessionMutation>,
    ) -> Result<Self, SessionReductionError> {
        let mut state = Self::new();
        for mutation in mutations {
            state.apply(&mutation)?;
        }
        Ok(state)
    }

    /// Returns the last accepted global sequence.
    pub fn sequence(&self) -> Sequence {
        self.sequence
    }

    /// Returns the sequence required by the next mutation.
    pub fn next_sequence(&self) -> Result<Sequence, SessionReductionError> {
        self.sequence
            .checked_next()
            .ok_or(SessionReductionError::ArithmeticOverflow)
    }

    /// Returns an entry by stable identifier.
    pub fn entry(&self, id: &EntryId) -> Option<&SessionEntry> {
        self.entries.get(id)
    }

    /// Returns every entry in ascending global sequence order.
    pub fn entries_in_sequence_order(&self) -> Vec<&SessionEntry> {
        self.entry_order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .collect()
    }

    /// Returns every operational record in ascending global sequence order.
    pub fn records_in_sequence_order(&self) -> &[OperationRecord] {
        &self.records
    }

    /// Returns every lane in durable insertion order.
    pub fn lanes(&self) -> Vec<crate::LaneState> {
        self.lane_order
            .iter()
            .filter_map(|name| {
                self.lanes.get(name).map(|leaf_id| crate::LaneState {
                    name: name.clone(),
                    leaf_id: leaf_id.clone(),
                })
            })
            .collect()
    }

    /// Returns a lane pointer, distinguishing a missing lane from an empty one.
    pub fn lane_leaf(&self, lane: &LaneName) -> Option<&Option<EntryId>> {
        self.lanes.get(lane)
    }

    /// Returns the authoritative log in ascending sequence order.
    pub fn log(&self) -> &[SessionMutation] {
        &self.log
    }

    /// Returns the latest global session name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the latest global label for an entry.
    pub fn label(&self, id: &EntryId) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    /// Returns all currently retained labels.
    pub fn labels(&self) -> &BTreeMap<EntryId, String> {
        &self.labels
    }

    /// Returns statistics derived only from entries and usage records.
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Scans one immutable branch from leaf toward root, inclusive.
    pub fn scan_branch_leaf_to_root(
        &self,
        leaf_id: &EntryId,
    ) -> Result<Vec<&SessionEntry>, SessionReductionError> {
        let mut path = Vec::new();
        let mut visited = BTreeSet::new();
        let mut current_id = Some(leaf_id);
        while let Some(id) = current_id {
            if !visited.insert(id.clone()) {
                return Err(SessionReductionError::MissingParent {
                    parent_id: id.clone(),
                });
            }
            let entry =
                self.entries
                    .get(id)
                    .ok_or_else(|| SessionReductionError::MissingParent {
                        parent_id: id.clone(),
                    })?;
            path.push(entry);
            current_id = entry.parent_id();
        }
        Ok(path)
    }

    /// Scans one immutable branch from root toward leaf, inclusive.
    pub fn scan_branch_root_to_leaf(
        &self,
        leaf_id: &EntryId,
    ) -> Result<Vec<&SessionEntry>, SessionReductionError> {
        let mut path = self.scan_branch_leaf_to_root(leaf_id)?;
        path.reverse();
        Ok(path)
    }

    /// Returns unresolved operation starts on a lane, newest first.
    pub fn open_operations(&self, lane: &LaneName) -> Vec<&OperationRecord> {
        self.open_operations
            .get(lane)
            .into_iter()
            .flat_map(|records| records.iter().rev())
            .collect()
    }

    /// Computes the lane's crash-recovery decision.
    pub fn recovery_decision(&self, lane: &LaneName) -> RecoveryDecision {
        let open = self.open_operations(lane);
        match open.as_slice() {
            [] => RecoveryDecision::Idle,
            [operation] => {
                let run_id = operation
                    .run_id()
                    .expect("operation-start records always expose their base id as a run id");
                let completed_steps = self
                    .records
                    .iter()
                    .filter(|record| {
                        record.lane() == lane
                            && record.sequence() > operation.sequence()
                            && record.run_id().as_ref() == Some(&run_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if completed_steps
                    .iter()
                    .any(|record| matches!(record, OperationRecord::AbortRequested { .. }))
                {
                    RecoveryDecision::Abandon {
                        operation: (*operation).clone(),
                        reason: PublicError {
                            code: "abort_requested".to_owned(),
                            message: "the interrupted operation has a durable abort request"
                                .to_owned(),
                            retryable: false,
                            provider_code: None,
                            status: None,
                            request_id: None,
                        },
                    }
                } else {
                    RecoveryDecision::Resume {
                        operation: (*operation).clone(),
                        completed_steps,
                    }
                }
            }
            _ => RecoveryDecision::Corrupt {
                open_operations: open.into_iter().cloned().collect(),
            },
        }
    }

    /// Builds re-sequenced entry, lane, name, and label mutations for a fork.
    pub fn create_fork_mutations(
        &self,
        position: &crate::ForkPosition,
    ) -> Result<Vec<SessionMutation>, crate::SessionError> {
        let (copied_entries, copied_lanes) = match position {
            crate::ForkPosition::WholeTree => (
                self.entries_in_sequence_order()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                self.lanes(),
            ),
            crate::ForkPosition::Before(target_id) | crate::ForkPosition::At(target_id) => {
                let target = self.entry(target_id).ok_or_else(|| {
                    crate::SessionError::new(
                        crate::SessionErrorKind::InvalidForkTarget,
                        format!("fork target does not exist: {target_id}"),
                    )
                })?;
                if !matches!(target, SessionEntry::Message { .. }) {
                    return Err(crate::SessionError::new(
                        crate::SessionErrorKind::InvalidForkTarget,
                        format!("fork target is not a message entry: {target_id}"),
                    ));
                }
                let leaf = match position {
                    crate::ForkPosition::Before(_) => target.parent_id().cloned(),
                    crate::ForkPosition::At(_) => Some(target_id.clone()),
                    crate::ForkPosition::WholeTree => unreachable!(),
                };
                let entries = match &leaf {
                    Some(id) => self
                        .scan_branch_root_to_leaf(id)
                        .map_err(crate::SessionError::from)?
                        .into_iter()
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                (
                    entries,
                    vec![crate::LaneState {
                        name: LaneName::new("main"),
                        leaf_id: leaf,
                    }],
                )
            }
        };

        let mut mutations = Vec::new();
        let mut sequence = Sequence::FIRST;
        for entry in &copied_entries {
            let base = EntryBaseForFork::from_entry(entry, sequence);
            mutations.push(SessionMutation::Entry {
                lane: None,
                entry: entry.clone().with_base(base.into()),
            });
            sequence = next_fork_sequence(sequence)?;
        }
        for lane in copied_lanes {
            mutations.push(SessionMutation::Lane {
                sequence,
                lane: lane.name,
                leaf_id: lane.leaf_id,
            });
            sequence = next_fork_sequence(sequence)?;
        }
        if let Some(name) = &self.name {
            mutations.push(SessionMutation::Fact {
                sequence,
                fact: SessionFact::Name {
                    name: Some(name.clone()),
                },
            });
            sequence = next_fork_sequence(sequence)?;
        }
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(entry.id()) {
                mutations.push(SessionMutation::Fact {
                    sequence,
                    fact: SessionFact::Label {
                        target_id: entry.id().clone(),
                        label: Some(label.clone()),
                    },
                });
                sequence = next_fork_sequence(sequence)?;
            }
        }
        Ok(mutations)
    }

    fn apply_entry(
        &mut self,
        lane: &Option<LaneName>,
        entry: &SessionEntry,
    ) -> Result<(), SessionReductionError> {
        self.validate_sequence(entry.sequence())?;
        self.validate_unused_id(entry.id().as_str())?;
        if let Some(parent_id) = entry.parent_id()
            && !self.entries.contains_key(parent_id)
        {
            return Err(SessionReductionError::MissingParent {
                parent_id: parent_id.clone(),
            });
        }
        if let Some(lane) = lane {
            let expected_parent = self
                .lanes
                .get(lane)
                .ok_or_else(|| SessionReductionError::MissingLane { lane: lane.clone() })?;
            if expected_parent.as_ref() != entry.parent_id() {
                return Err(SessionReductionError::LaneChainMismatch {
                    lane: lane.clone(),
                    expected_parent: expected_parent.clone(),
                    actual_parent: entry.parent_id().cloned(),
                });
            }
        }
        let message_count = if matches!(entry, SessionEntry::Message { .. }) {
            self.stats
                .message_count
                .checked_add(1)
                .ok_or(SessionReductionError::ArithmeticOverflow)?
        } else {
            self.stats.message_count
        };

        self.sequence = entry.sequence();
        self.used_ids.insert(entry.id().as_str().to_owned());
        self.entry_order.push(entry.id().clone());
        self.entries.insert(entry.id().clone(), entry.clone());
        self.pending_queue_entries
            .retain(|(_, entry_id), _| entry_id != entry.id());
        if let Some(lane) = lane {
            self.lanes.insert(lane.clone(), Some(entry.id().clone()));
        }
        self.stats.message_count = message_count;
        Ok(())
    }

    fn apply_record(&mut self, record: &OperationRecord) -> Result<(), SessionReductionError> {
        self.validate_sequence(record.sequence())?;
        if !self.lanes.contains_key(record.lane()) {
            return Err(SessionReductionError::MissingLane {
                lane: record.lane().clone(),
            });
        }
        self.validate_unused_id(record.base().id.as_str())?;
        if let OperationRecord::ToolStarted {
            assistant_entry_id,
            tool_index,
            call,
            ..
        } = record
        {
            let key = (
                record.lane().clone(),
                assistant_entry_id.clone(),
                *tool_index,
            );
            if self.tool_invocations.contains(&key) {
                return Err(SessionReductionError::DuplicateToolInvocation {
                    assistant_entry_id: assistant_entry_id.clone(),
                    tool_index: *tool_index,
                });
            }
            self.validate_tool_reference(assistant_entry_id, *tool_index, call)?;
        }
        if let OperationRecord::QueueCancelled {
            run_id, entry_id, ..
        } = record
        {
            let key = (record.lane().clone(), entry_id.clone());
            let enqueue = self.queue_enqueue_history.get(&key);
            if enqueue.is_none_or(|enqueue| enqueue.run_id.as_ref() != run_id.as_ref())
                || self.entries.contains_key(entry_id)
            {
                return Err(SessionReductionError::MissingQueuedEntry {
                    entry_id: entry_id.clone(),
                });
            }
        }
        let new_stats = self.stats_after_record(record)?;

        self.sequence = record.sequence();
        self.used_ids.insert(record.base().id.as_str().to_owned());
        self.records.push(record.clone());
        match record {
            OperationRecord::Started { .. } => self
                .open_operations
                .entry(record.lane().clone())
                .or_default()
                .push(record.clone()),
            OperationRecord::Finished { run_id, .. } => {
                if let Some(open) = self.open_operations.get_mut(record.lane())
                    && let Some(index) = open.iter().position(|started| {
                        started
                            .run_id()
                            .is_some_and(|candidate| candidate == *run_id)
                    })
                {
                    open.remove(index);
                }
            }
            OperationRecord::ToolStarted {
                assistant_entry_id,
                tool_index,
                ..
            } => {
                self.tool_invocations.insert((
                    record.lane().clone(),
                    assistant_entry_id.clone(),
                    *tool_index,
                ));
            }
            OperationRecord::QueueEnqueued { run_id, target, .. } => {
                let key = (record.lane().clone(), target.id().clone());
                let enqueue = PendingQueueEntry {
                    run_id: run_id.clone(),
                };
                self.queue_enqueue_history
                    .insert(key.clone(), enqueue.clone());
                if !self.entries.contains_key(target.id())
                    && !self.cancelled_queue_entries.contains(&key)
                {
                    self.pending_queue_entries.insert(key, enqueue);
                }
            }
            OperationRecord::QueueCancelled { entry_id, .. } => {
                let key = (record.lane().clone(), entry_id.clone());
                self.cancelled_queue_entries.insert(key.clone());
                self.pending_queue_entries.remove(&key);
            }
            OperationRecord::AbortRequested { .. }
            | OperationRecord::StepAttempt { .. }
            | OperationRecord::WriteDeferred { .. }
            | OperationRecord::Usage { .. } => {}
        }
        self.stats = new_stats;
        Ok(())
    }

    fn apply_lane(
        &mut self,
        sequence: Sequence,
        lane: &LaneName,
        leaf_id: &Option<EntryId>,
    ) -> Result<(), SessionReductionError> {
        self.validate_sequence(sequence)?;
        if let Some(target_id) = leaf_id
            && !self.entries.contains_key(target_id)
        {
            return Err(SessionReductionError::MissingLaneTarget {
                target_id: target_id.clone(),
            });
        }
        self.sequence = sequence;
        if !self.lanes.contains_key(lane) {
            self.lane_order.push(lane.clone());
        }
        self.lanes.insert(lane.clone(), leaf_id.clone());
        Ok(())
    }

    fn apply_fact(
        &mut self,
        sequence: Sequence,
        fact: &SessionFact,
    ) -> Result<(), SessionReductionError> {
        self.validate_sequence(sequence)?;
        if let SessionFact::Label { target_id, .. } = fact
            && !self.entries.contains_key(target_id)
        {
            return Err(SessionReductionError::MissingLabelTarget {
                target_id: target_id.clone(),
            });
        }
        self.sequence = sequence;
        match fact {
            SessionFact::Name { name } => self.name = name.clone(),
            SessionFact::Label { target_id, label } => match label {
                Some(label) => {
                    self.labels.insert(target_id.clone(), label.clone());
                }
                None => {
                    self.labels.remove(target_id);
                }
            },
        }
        Ok(())
    }

    fn validate_sequence(&self, actual: Sequence) -> Result<(), SessionReductionError> {
        let expected = self.next_sequence()?;
        if actual == expected {
            Ok(())
        } else {
            Err(SessionReductionError::SequenceGap { expected, actual })
        }
    }

    fn validate_unused_id(&self, id: &str) -> Result<(), SessionReductionError> {
        if self.used_ids.contains(id) {
            Err(SessionReductionError::DuplicateId { id: id.to_owned() })
        } else {
            Ok(())
        }
    }

    fn validate_tool_reference(
        &self,
        assistant_entry_id: &EntryId,
        tool_index: u32,
        call: &crate::ToolCallIdentity,
    ) -> Result<(), SessionReductionError> {
        if let Some(entry) = self.entries.get(assistant_entry_id) {
            let SessionEntry::Message {
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } = entry
            else {
                return Err(SessionReductionError::ToolIdentityMismatch {
                    assistant_entry_id: assistant_entry_id.clone(),
                    tool_index,
                });
            };
            let indexed_call = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call, .. } => Some(call),
                    ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Thinking { .. } => None,
                })
                .nth(tool_index as usize);
            if indexed_call
                .is_some_and(|candidate| candidate.id == call.id && candidate.name == call.name)
            {
                return Ok(());
            }
            return Err(SessionReductionError::ToolIdentityMismatch {
                assistant_entry_id: assistant_entry_id.clone(),
                tool_index,
            });
        }
        Err(SessionReductionError::MissingToolAssistant {
            assistant_entry_id: assistant_entry_id.clone(),
        })
    }

    fn stats_after_record(
        &self,
        record: &OperationRecord,
    ) -> Result<SessionStats, SessionReductionError> {
        let mut stats = self.stats.clone();
        let OperationRecord::Usage {
            usage,
            cost,
            adjustment,
            ..
        } = record
        else {
            return Ok(stats);
        };

        checked_add(
            &mut stats.cached_tokens,
            usage.cache_read_tokens.unwrap_or(0),
        )?;
        checked_add(
            &mut stats.uncached_tokens,
            u128::from(usage.input_tokens) + u128::from(usage.cache_write_tokens.unwrap_or(0)),
        )?;
        let total_tokens = usage
            .total_tokens
            .map(u128::from)
            .unwrap_or_else(|| usage.request_input_tokens() + u128::from(usage.output_tokens));
        checked_add(&mut stats.total_tokens, total_tokens)?;
        if let Some(cost) = cost {
            checked_add_cost(&mut stats, cost)?;
        }
        if let Some(adjustment) = adjustment {
            stats.cached_tokens = stats
                .cached_tokens
                .checked_add(adjustment.cache_read_tokens)
                .ok_or(SessionReductionError::ArithmeticOverflow)?;
            stats.uncached_tokens = stats
                .uncached_tokens
                .checked_add(adjustment.input_tokens)
                .and_then(|value| value.checked_add(adjustment.cache_write_tokens))
                .ok_or(SessionReductionError::ArithmeticOverflow)?;
            stats.total_tokens = stats
                .total_tokens
                .checked_add(adjustment.total_tokens)
                .ok_or(SessionReductionError::ArithmeticOverflow)?;
            if let Some(cost) = &adjustment.cost {
                checked_add_cost(&mut stats, cost)?;
            }
        }
        Ok(stats)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingQueueEntry {
    run_id: Option<agentprism_ai::RunId>,
}

impl SessionReducer for SessionState {
    fn apply(&mut self, mutation: &SessionMutation) -> Result<(), SessionReductionError> {
        match mutation {
            SessionMutation::Entry { lane, entry } => self.apply_entry(lane, entry)?,
            SessionMutation::Record { record } => self.apply_record(record)?,
            SessionMutation::Lane {
                sequence,
                lane,
                leaf_id,
            } => self.apply_lane(*sequence, lane, leaf_id)?,
            SessionMutation::Fact { sequence, fact } => self.apply_fact(*sequence, fact)?,
        }
        self.log.push(mutation.clone());
        Ok(())
    }

    fn state(&self) -> &SessionState {
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct EntryBaseForFork {
    id: EntryId,
    sequence: Sequence,
    parent_id: Option<EntryId>,
    timestamp: agentprism_ai::Timestamp,
}

impl EntryBaseForFork {
    fn from_entry(entry: &SessionEntry, sequence: Sequence) -> Self {
        Self {
            id: entry.id().clone(),
            sequence,
            parent_id: entry.parent_id().cloned(),
            timestamp: entry.base().timestamp,
        }
    }
}

impl From<EntryBaseForFork> for crate::EntryBase {
    fn from(base: EntryBaseForFork) -> Self {
        Self {
            id: base.id,
            sequence: base.sequence,
            parent_id: base.parent_id,
            timestamp: base.timestamp,
        }
    }
}

fn next_fork_sequence(sequence: Sequence) -> Result<Sequence, crate::SessionError> {
    sequence.checked_next().ok_or_else(|| {
        crate::SessionError::new(
            crate::SessionErrorKind::Corruption,
            "fork mutation sequence overflow",
        )
    })
}

fn checked_add(target: &mut i128, value: impl TryInto<i128>) -> Result<(), SessionReductionError> {
    let value = value
        .try_into()
        .map_err(|_| SessionReductionError::ArithmeticOverflow)?;
    *target = target
        .checked_add(value)
        .ok_or(SessionReductionError::ArithmeticOverflow)?;
    Ok(())
}

fn checked_add_cost(
    stats: &mut SessionStats,
    cost: &agentprism_ai::Cost,
) -> Result<(), SessionReductionError> {
    let total = stats
        .cost_micros_by_currency
        .entry(cost.currency.as_str().to_owned())
        .or_insert(0);
    *total = total
        .checked_add(cost.micros)
        .ok_or(SessionReductionError::ArithmeticOverflow)?;
    Ok(())
}
