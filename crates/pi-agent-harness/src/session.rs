//! Serialized durable-session operations used by harness policies.

use crate::{HarnessIdGenerator, LocalHarnessIdGenerator};
use futures_util::lock::Mutex;
use pi_agent_env::{Clock, LocalClock};
use pi_agent_session::{
    AppendReceipt, CompactionReason, EntryBase, EntryId, LaneName, LocalSessionStorage,
    OperationIntent, OperationOutcome, OperationRecord, OperationRecordBase, OperationRecordId,
    OperationStep, ProvisionedEntry, QueueKind, Sequence, SessionEntry, SessionError,
    SessionErrorKind, SessionMutation, SessionState, SessionStorage, ToolCallIdentity,
    ToolReplayPolicy, UsageAttribution,
};
use pi_ai::{
    AssistantFinishReason, AssistantMessage, Cost, Message, PublicError, RunId, Timestamp, Usage,
    VersionedExtension,
};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

/// Cloneable serialized facade over one durable session lane.
///
/// The facade owns no cached state. Every mutation is built from a fresh
/// storage snapshot while an executor-neutral mutex serializes this facade's
/// read-modify-append transactions.
pub struct Session {
    storage: Arc<dyn SessionStorage>,
    lane: LaneName,
    ids: Arc<dyn HarnessIdGenerator>,
    clock: Arc<dyn Clock>,
    mutation_lock: Mutex<()>,
}

impl Session {
    /// Binds durable storage, one lane, an identifier source, and a host clock.
    pub fn new(
        storage: Arc<dyn SessionStorage>,
        lane: LaneName,
        ids: Arc<dyn HarnessIdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            storage,
            lane,
            ids,
            clock,
            mutation_lock: Mutex::new(()),
        }
    }

    /// Returns the selected durable lane.
    pub fn lane(&self) -> &LaneName {
        &self.lane
    }

    /// Loads the current authoritative session projection.
    pub async fn load_state(&self) -> Result<SessionState, SessionError> {
        self.storage.load_state().await
    }

    /// Reconstructs the selected branch in chronological order.
    pub async fn branch_entries(&self) -> Result<Vec<SessionEntry>, SessionError> {
        let state = self.storage.load_state().await?;
        let leaf = state.lane_leaf(&self.lane).ok_or_else(|| {
            SessionError::new(
                SessionErrorKind::InvalidLane,
                format!("lane {} does not exist", self.lane),
            )
        })?;
        match leaf {
            Some(leaf_id) => Ok(state
                .scan_branch_root_to_leaf(leaf_id)
                .map_err(SessionError::from)?
                .into_iter()
                .cloned()
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// Allocates a stable future entry identifier.
    pub fn next_entry_id(&self, kind: &'static str) -> EntryId {
        EntryId::new(self.ids.next_id(kind))
    }

    /// Starts one standalone operation and returns its run identity.
    pub(crate) async fn start_operation(
        &self,
        intent: OperationIntent,
    ) -> Result<RunId, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        ensure_lane_idle(&state, &self.lane)?;
        let run_id = RunId::new(self.ids.next_id("operation"));
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        let record = OperationRecord::Started {
            base: OperationRecordBase {
                id: OperationRecordId::new(run_id.as_str()),
                sequence,
                lane: self.lane.clone(),
                timestamp: self.clock.now(),
            },
            source_leaf_id: state.lane_leaf(&self.lane).cloned().flatten(),
            intent,
        };
        self.storage
            .append(state.sequence(), vec![SessionMutation::Record { record }])
            .await?;
        Ok(run_id)
    }

    /// Returns the only open operation on the lane, if one exists.
    pub(crate) async fn open_operation(&self) -> Result<Option<OperationRecord>, SessionError> {
        let state = self.storage.load_state().await?;
        let open = state.open_operations(&self.lane);
        match open.as_slice() {
            [] => Ok(None),
            [operation] => Ok(Some((*operation).clone())),
            _ => Err(SessionError::new(
                SessionErrorKind::Corruption,
                format!("lane {} has multiple open operations", self.lane),
            )),
        }
    }

    /// Commits provisioned run input in intent order before the first assistant attempt.
    pub(crate) async fn commit_provisioned_entries(
        &self,
        entries: Vec<ProvisionedEntry>,
    ) -> Result<(), SessionError> {
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        let mut parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::with_capacity(entries.len());
        for provisioned in entries {
            let id = provisioned.id().clone();
            mutations.push(SessionMutation::Entry {
                lane: Some(self.lane.clone()),
                entry: provisioned.materialize(next, parent_id, self.clock.now()),
            });
            parent_id = Some(id);
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        self.storage.append(state.sequence(), mutations).await?;
        Ok(())
    }

    /// Durably acknowledges one queue item before the harness reports success.
    pub(crate) async fn enqueue(
        &self,
        run_id: Option<RunId>,
        queue: QueueKind,
        target: ProvisionedEntry,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_queue_owner(&state, &self.lane, run_id.as_ref(), queue)?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::QueueEnqueued {
                        base: self.record_base(sequence),
                        run_id,
                        queue,
                        target,
                    },
                }],
            )
            .await
    }

    /// Cancels one durably pending queue item.
    pub(crate) async fn cancel_queued(
        &self,
        run_id: Option<RunId>,
        entry_id: EntryId,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_pending_queue(&state, &self.lane, run_id.as_ref(), &entry_id)?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::QueueCancelled {
                        base: self.record_base(sequence),
                        run_id,
                        entry_id,
                    },
                }],
            )
            .await
    }

    /// Records a durable abort request for the current operation.
    pub(crate) async fn request_abort(&self, run_id: RunId) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        if operation_has_abort(&state, &self.lane, &run_id) {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("operation {run_id} already has an abort request"),
            ));
        }
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::AbortRequested {
                        base: self.record_base(sequence),
                        run_id,
                    },
                }],
            )
            .await
    }

    /// Records a started tool invocation before executing its side effects.
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable tool-start record"
    )]
    pub(crate) async fn append_tool_started(
        &self,
        run_id: RunId,
        assistant_entry_id: EntryId,
        tool_index: u32,
        call: ToolCallIdentity,
        effective_args: serde_json::Value,
        result_entry_id: EntryId,
        replay: ToolReplayPolicy,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::ToolStarted {
            base,
            run_id,
            assistant_entry_id,
            tool_index,
            call,
            effective_args,
            result_entry_id,
            replay,
        })
        .await
    }

    /// Atomically commits one non-assistant agent record and optional tool usage.
    pub(crate) async fn commit_agent_record(
        &self,
        run_id: RunId,
        entry_id: EntryId,
        message: pi_agent_core::AgentRecord,
        terminate: bool,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::new();
        if let pi_agent_core::AgentRecord::Llm(Message::ToolResult(result)) = &message
            && let Some(usage) = result.usage.clone()
        {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::Tool {
                        run_id,
                        entry_id: entry_id.clone(),
                        tool_call_id: result.tool_call_id.clone(),
                    },
                    usage,
                    cost: None,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::Message {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id,
                    timestamp: self.clock.now(),
                },
                message,
                terminate,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    /// Appends one durable model-backed step attempt.
    pub(crate) async fn append_step_attempt(
        &self,
        run_id: RunId,
        step: OperationStep,
        attempt: u32,
        result_entry_id: EntryId,
        compaction_reason: Option<CompactionReason>,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::StepAttempt {
            base,
            run_id,
            step,
            attempt,
            result_entry_id,
            compaction_reason,
        })
        .await
    }

    /// Appends usage for one model-backed attempt without committing its entry.
    pub(crate) async fn append_usage(
        &self,
        attribution: UsageAttribution,
        usage: Usage,
        cost: Option<Cost>,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::Usage {
            base,
            attribution,
            usage,
            cost,
            adjustment: None,
        })
        .await
    }

    /// Atomically commits assistant usage followed by its durable message entry.
    pub(crate) async fn commit_assistant(
        &self,
        run_id: RunId,
        attempt: u32,
        entry_id: EntryId,
        message: AssistantMessage,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let usage = message.usage.clone();
        let cost = message.cost.clone();
        let stop_reason = message.finish.reason;
        let usage_record = SessionMutation::Record {
            record: OperationRecord::Usage {
                base: self.record_base(next),
                attribution: UsageAttribution::Assistant {
                    run_id,
                    entry_id: entry_id.clone(),
                    attempt,
                    stop_reason,
                },
                usage,
                cost,
                adjustment: None,
            },
        };
        next = next.checked_next().ok_or_else(sequence_overflow)?;
        let entry = SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::Message {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id,
                    timestamp: self.clock.now(),
                },
                message: pi_agent_core::AgentRecord::Llm(Message::Assistant(message)),
                terminate: false,
            },
        };
        self.storage
            .append(state.sequence(), vec![usage_record, entry])
            .await
    }

    /// Atomically commits summary usage followed by a compaction branch entry.
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable compaction entry"
    )]
    pub(crate) async fn commit_compaction(
        &self,
        run_id: RunId,
        attempt: u32,
        entry_id: EntryId,
        summary: String,
        retained_tail: Vec<pi_agent_core::AgentRecord>,
        tokens_before: u64,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
        cost: Option<Cost>,
        stop_reason: AssistantFinishReason,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::new();
        if let Some(usage_value) = usage.clone() {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::Compaction {
                        run_id,
                        entry_id: entry_id.clone(),
                        attempt,
                        stop_reason,
                    },
                    usage: usage_value,
                    cost,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        let entry = SessionEntry::Compaction {
            base: EntryBase {
                id: entry_id,
                sequence: next,
                parent_id,
                timestamp: self.clock.now(),
            },
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
        };
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry,
        });
        self.storage.append(state.sequence(), mutations).await
    }

    /// Atomically moves to a target, commits summary usage and entry, and advances the lane.
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable branch-summary entry"
    )]
    pub(crate) async fn commit_branch_summary(
        &self,
        run_id: RunId,
        attempt: u32,
        target_id: Option<EntryId>,
        entry_id: EntryId,
        from_id: EntryId,
        summary: String,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
        cost: Option<Cost>,
        stop_reason: AssistantFinishReason,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("entry {target} not found"),
            ));
        }
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = vec![SessionMutation::Lane {
            sequence: next,
            lane: self.lane.clone(),
            leaf_id: target_id.clone(),
        }];
        next = next.checked_next().ok_or_else(sequence_overflow)?;
        if let Some(usage_value) = usage.clone() {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::BranchSummary {
                        run_id,
                        entry_id: entry_id.clone(),
                        attempt,
                        stop_reason,
                    },
                    usage: usage_value,
                    cost,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::BranchSummary {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id: target_id,
                    timestamp: self.clock.now(),
                },
                from_id,
                summary,
                details,
                usage,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    /// Atomically moves the lane without a summary entry.
    pub(crate) async fn move_lane(
        &self,
        target_id: Option<EntryId>,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("entry {target} not found"),
            ));
        }
        let mutation = SessionMutation::Lane {
            sequence: state.next_sequence().map_err(SessionError::from)?,
            lane: self.lane.clone(),
            leaf_id: target_id,
        };
        self.storage.append(state.sequence(), vec![mutation]).await
    }

    /// Closes an operation with one durable terminal record.
    pub(crate) async fn finish_operation(
        &self,
        run_id: RunId,
        outcome: OperationOutcome,
        error: Option<PublicError>,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let pending = if outcome == OperationOutcome::Completed {
            Vec::new()
        } else {
            pending_operation_queues(&state, &self.lane, &run_id)
        };
        let mut sequence = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::with_capacity(pending.len().saturating_add(1));
        for entry_id in pending {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::QueueCancelled {
                    base: self.record_base(sequence),
                    run_id: Some(run_id.clone()),
                    entry_id,
                },
            });
            sequence = sequence.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Record {
            record: OperationRecord::Finished {
                base: self.record_base(sequence),
                run_id,
                outcome,
                error,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    async fn append_record(
        &self,
        build: impl FnOnce(OperationRecordBase) -> OperationRecord,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        let record = build(self.record_base(sequence));
        if let Some(run_id) = record.run_id() {
            validate_open_operation(&state, &self.lane, &run_id)?;
        }
        let mutation = SessionMutation::Record { record };
        self.storage.append(state.sequence(), vec![mutation]).await
    }

    fn record_base(&self, sequence: Sequence) -> OperationRecordBase {
        OperationRecordBase {
            id: OperationRecordId::new(self.ids.next_id("record")),
            sequence,
            lane: self.lane.clone(),
            timestamp: self.clock.now(),
        }
    }
}

/// Cloneable single-threaded serialized facade over one durable session lane.
///
/// Unlike [`Session`], this facade retains `Rc`-owned storage, clock, and ID
/// capabilities and therefore accepts genuinely non-`Send` host state.
pub struct LocalSession {
    storage: Rc<dyn LocalSessionStorage>,
    lane: LaneName,
    ids: Rc<dyn LocalHarnessIdGenerator>,
    clock: Rc<dyn LocalClock>,
    mutation_lock: Mutex<()>,
}

impl LocalSession {
    /// Binds local durable storage, one lane, an identifier source, and a host clock.
    pub fn new(
        storage: Rc<dyn LocalSessionStorage>,
        lane: LaneName,
        ids: Rc<dyn LocalHarnessIdGenerator>,
        clock: Rc<dyn LocalClock>,
    ) -> Self {
        Self {
            storage,
            lane,
            ids,
            clock,
            mutation_lock: Mutex::new(()),
        }
    }

    /// Returns the selected durable lane.
    pub fn lane(&self) -> &LaneName {
        &self.lane
    }

    /// Loads the current authoritative session projection.
    pub async fn load_state(&self) -> Result<SessionState, SessionError> {
        self.storage.load_state().await
    }

    /// Reconstructs the selected branch in chronological order.
    pub async fn branch_entries(&self) -> Result<Vec<SessionEntry>, SessionError> {
        let state = self.storage.load_state().await?;
        let leaf = state.lane_leaf(&self.lane).ok_or_else(|| {
            SessionError::new(
                SessionErrorKind::InvalidLane,
                format!("lane {} does not exist", self.lane),
            )
        })?;
        match leaf {
            Some(leaf_id) => Ok(state
                .scan_branch_root_to_leaf(leaf_id)
                .map_err(SessionError::from)?
                .into_iter()
                .cloned()
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// Allocates a stable future entry identifier.
    pub fn next_entry_id(&self, kind: &'static str) -> EntryId {
        EntryId::new(self.ids.next_id(kind))
    }

    pub(crate) async fn start_operation(
        &self,
        intent: OperationIntent,
    ) -> Result<RunId, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        ensure_lane_idle(&state, &self.lane)?;
        let run_id = RunId::new(self.ids.next_id("operation"));
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        let record = OperationRecord::Started {
            base: OperationRecordBase {
                id: OperationRecordId::new(run_id.as_str()),
                sequence,
                lane: self.lane.clone(),
                timestamp: self.clock.now(),
            },
            source_leaf_id: state.lane_leaf(&self.lane).cloned().flatten(),
            intent,
        };
        self.storage
            .append(state.sequence(), vec![SessionMutation::Record { record }])
            .await?;
        Ok(run_id)
    }

    pub(crate) async fn open_operation(&self) -> Result<Option<OperationRecord>, SessionError> {
        let state = self.storage.load_state().await?;
        let open = state.open_operations(&self.lane);
        match open.as_slice() {
            [] => Ok(None),
            [operation] => Ok(Some((*operation).clone())),
            _ => Err(SessionError::new(
                SessionErrorKind::Corruption,
                format!("lane {} has multiple open operations", self.lane),
            )),
        }
    }

    pub(crate) async fn commit_provisioned_entries(
        &self,
        entries: Vec<ProvisionedEntry>,
    ) -> Result<(), SessionError> {
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        let mut parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::with_capacity(entries.len());
        for provisioned in entries {
            let id = provisioned.id().clone();
            mutations.push(SessionMutation::Entry {
                lane: Some(self.lane.clone()),
                entry: provisioned.materialize(next, parent_id, self.clock.now()),
            });
            parent_id = Some(id);
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        self.storage.append(state.sequence(), mutations).await?;
        Ok(())
    }

    pub(crate) async fn enqueue(
        &self,
        run_id: Option<RunId>,
        queue: QueueKind,
        target: ProvisionedEntry,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_queue_owner(&state, &self.lane, run_id.as_ref(), queue)?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::QueueEnqueued {
                        base: self.record_base(sequence),
                        run_id,
                        queue,
                        target,
                    },
                }],
            )
            .await
    }

    pub(crate) async fn cancel_queued(
        &self,
        run_id: Option<RunId>,
        entry_id: EntryId,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_pending_queue(&state, &self.lane, run_id.as_ref(), &entry_id)?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::QueueCancelled {
                        base: self.record_base(sequence),
                        run_id,
                        entry_id,
                    },
                }],
            )
            .await
    }

    pub(crate) async fn request_abort(&self, run_id: RunId) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        if operation_has_abort(&state, &self.lane, &run_id) {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("operation {run_id} already has an abort request"),
            ));
        }
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Record {
                    record: OperationRecord::AbortRequested {
                        base: self.record_base(sequence),
                        run_id,
                    },
                }],
            )
            .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable tool-start record"
    )]
    pub(crate) async fn append_tool_started(
        &self,
        run_id: RunId,
        assistant_entry_id: EntryId,
        tool_index: u32,
        call: ToolCallIdentity,
        effective_args: serde_json::Value,
        result_entry_id: EntryId,
        replay: ToolReplayPolicy,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::ToolStarted {
            base,
            run_id,
            assistant_entry_id,
            tool_index,
            call,
            effective_args,
            result_entry_id,
            replay,
        })
        .await
    }

    pub(crate) async fn commit_agent_record(
        &self,
        run_id: RunId,
        entry_id: EntryId,
        message: pi_agent_core::AgentRecord,
        terminate: bool,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::new();
        if let pi_agent_core::AgentRecord::Llm(Message::ToolResult(result)) = &message
            && let Some(usage) = result.usage.clone()
        {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::Tool {
                        run_id,
                        entry_id: entry_id.clone(),
                        tool_call_id: result.tool_call_id.clone(),
                    },
                    usage,
                    cost: None,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::Message {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id,
                    timestamp: self.clock.now(),
                },
                message,
                terminate,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    pub(crate) async fn append_step_attempt(
        &self,
        run_id: RunId,
        step: OperationStep,
        attempt: u32,
        result_entry_id: EntryId,
        compaction_reason: Option<CompactionReason>,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::StepAttempt {
            base,
            run_id,
            step,
            attempt,
            result_entry_id,
            compaction_reason,
        })
        .await
    }

    pub(crate) async fn append_usage(
        &self,
        attribution: UsageAttribution,
        usage: Usage,
        cost: Option<Cost>,
    ) -> Result<AppendReceipt, SessionError> {
        self.append_record(|base| OperationRecord::Usage {
            base,
            attribution,
            usage,
            cost,
            adjustment: None,
        })
        .await
    }

    pub(crate) async fn commit_assistant(
        &self,
        run_id: RunId,
        attempt: u32,
        entry_id: EntryId,
        message: AssistantMessage,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let usage_record = SessionMutation::Record {
            record: OperationRecord::Usage {
                base: self.record_base(next),
                attribution: UsageAttribution::Assistant {
                    run_id,
                    entry_id: entry_id.clone(),
                    attempt,
                    stop_reason: message.finish.reason,
                },
                usage: message.usage.clone(),
                cost: message.cost.clone(),
                adjustment: None,
            },
        };
        next = next.checked_next().ok_or_else(sequence_overflow)?;
        let entry = SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::Message {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id,
                    timestamp: self.clock.now(),
                },
                message: pi_agent_core::AgentRecord::Llm(Message::Assistant(message)),
                terminate: false,
            },
        };
        self.storage
            .append(state.sequence(), vec![usage_record, entry])
            .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable compaction entry"
    )]
    pub(crate) async fn commit_compaction(
        &self,
        run_id: RunId,
        attempt: u32,
        entry_id: EntryId,
        summary: String,
        retained_tail: Vec<pi_agent_core::AgentRecord>,
        tokens_before: u64,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
        cost: Option<Cost>,
        stop_reason: AssistantFinishReason,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let parent_id = lane_leaf(&state, &self.lane)?;
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::new();
        if let Some(usage_value) = usage.clone() {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::Compaction {
                        run_id,
                        entry_id: entry_id.clone(),
                        attempt,
                        stop_reason,
                    },
                    usage: usage_value,
                    cost,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::Compaction {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id,
                    timestamp: self.clock.now(),
                },
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the adopted durable branch-summary entry"
    )]
    pub(crate) async fn commit_branch_summary(
        &self,
        run_id: RunId,
        attempt: u32,
        target_id: Option<EntryId>,
        entry_id: EntryId,
        from_id: EntryId,
        summary: String,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
        cost: Option<Cost>,
        stop_reason: AssistantFinishReason,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("entry {target} not found"),
            ));
        }
        let mut next = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = vec![SessionMutation::Lane {
            sequence: next,
            lane: self.lane.clone(),
            leaf_id: target_id.clone(),
        }];
        next = next.checked_next().ok_or_else(sequence_overflow)?;
        if let Some(usage_value) = usage.clone() {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::Usage {
                    base: self.record_base(next),
                    attribution: UsageAttribution::BranchSummary {
                        run_id,
                        entry_id: entry_id.clone(),
                        attempt,
                        stop_reason,
                    },
                    usage: usage_value,
                    cost,
                    adjustment: None,
                },
            });
            next = next.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Entry {
            lane: Some(self.lane.clone()),
            entry: SessionEntry::BranchSummary {
                base: EntryBase {
                    id: entry_id,
                    sequence: next,
                    parent_id: target_id,
                    timestamp: self.clock.now(),
                },
                from_id,
                summary,
                details,
                usage,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    pub(crate) async fn move_lane(
        &self,
        target_id: Option<EntryId>,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("entry {target} not found"),
            ));
        }
        self.storage
            .append(
                state.sequence(),
                vec![SessionMutation::Lane {
                    sequence: state.next_sequence().map_err(SessionError::from)?,
                    lane: self.lane.clone(),
                    leaf_id: target_id,
                }],
            )
            .await
    }

    pub(crate) async fn finish_operation(
        &self,
        run_id: RunId,
        outcome: OperationOutcome,
        error: Option<PublicError>,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        validate_open_operation(&state, &self.lane, &run_id)?;
        let pending = if outcome == OperationOutcome::Completed {
            Vec::new()
        } else {
            pending_operation_queues(&state, &self.lane, &run_id)
        };
        let mut sequence = state.next_sequence().map_err(SessionError::from)?;
        let mut mutations = Vec::with_capacity(pending.len().saturating_add(1));
        for entry_id in pending {
            mutations.push(SessionMutation::Record {
                record: OperationRecord::QueueCancelled {
                    base: self.record_base(sequence),
                    run_id: Some(run_id.clone()),
                    entry_id,
                },
            });
            sequence = sequence.checked_next().ok_or_else(sequence_overflow)?;
        }
        mutations.push(SessionMutation::Record {
            record: OperationRecord::Finished {
                base: self.record_base(sequence),
                run_id,
                outcome,
                error,
            },
        });
        self.storage.append(state.sequence(), mutations).await
    }

    async fn append_record(
        &self,
        build: impl FnOnce(OperationRecordBase) -> OperationRecord,
    ) -> Result<AppendReceipt, SessionError> {
        let _guard = self.mutation_lock.lock().await;
        let state = self.storage.load_state().await?;
        let sequence = state.next_sequence().map_err(SessionError::from)?;
        let record = build(self.record_base(sequence));
        if let Some(run_id) = record.run_id() {
            validate_open_operation(&state, &self.lane, &run_id)?;
        }
        self.storage
            .append(state.sequence(), vec![SessionMutation::Record { record }])
            .await
    }

    fn record_base(&self, sequence: Sequence) -> OperationRecordBase {
        OperationRecordBase {
            id: OperationRecordId::new(self.ids.next_id("record")),
            sequence,
            lane: self.lane.clone(),
            timestamp: self.clock.now(),
        }
    }
}

fn lane_leaf(state: &SessionState, lane: &LaneName) -> Result<Option<EntryId>, SessionError> {
    state.lane_leaf(lane).cloned().ok_or_else(|| {
        SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("lane {lane} does not exist"),
        )
    })
}

fn ensure_lane_idle(state: &SessionState, lane: &LaneName) -> Result<(), SessionError> {
    if state.lane_leaf(lane).is_none() {
        return Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("lane {lane} does not exist"),
        ));
    }
    if state.open_operations(lane).is_empty() {
        Ok(())
    } else {
        Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("lane {lane} already has an open operation"),
        ))
    }
}

fn validate_open_operation(
    state: &SessionState,
    lane: &LaneName,
    run_id: &RunId,
) -> Result<(), SessionError> {
    let open = state.open_operations(lane);
    if open.len() == 1 && open[0].run_id().as_ref() == Some(run_id) {
        Ok(())
    } else {
        Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("lane {lane} does not have active operation {run_id}"),
        ))
    }
}

fn operation_has_abort(state: &SessionState, lane: &LaneName, run_id: &RunId) -> bool {
    state.records_in_sequence_order().iter().any(|record| {
        matches!(
            record,
            OperationRecord::AbortRequested {
                run_id: candidate,
                ..
            } if record.lane() == lane && candidate == run_id
        )
    })
}

fn pending_operation_queues(state: &SessionState, lane: &LaneName, run_id: &RunId) -> Vec<EntryId> {
    let mut pending = BTreeMap::<EntryId, Option<RunId>>::new();
    for record in state.records_in_sequence_order() {
        if record.lane() != lane {
            continue;
        }
        match record {
            OperationRecord::QueueEnqueued {
                run_id: owner,
                queue: QueueKind::Steer | QueueKind::FollowUp,
                target,
                ..
            } if owner.as_ref() == Some(run_id) => {
                pending.insert(target.id().clone(), owner.clone());
            }
            OperationRecord::QueueCancelled {
                run_id: owner,
                entry_id,
                ..
            } if owner.as_ref() == Some(run_id) => {
                pending.remove(entry_id);
            }
            OperationRecord::Started { .. }
            | OperationRecord::AbortRequested { .. }
            | OperationRecord::Finished { .. }
            | OperationRecord::StepAttempt { .. }
            | OperationRecord::ToolStarted { .. }
            | OperationRecord::QueueEnqueued { .. }
            | OperationRecord::QueueCancelled { .. }
            | OperationRecord::WriteDeferred { .. }
            | OperationRecord::Usage { .. } => {}
        }
    }
    pending
        .into_keys()
        .filter(|entry_id| state.entry(entry_id).is_none())
        .collect()
}

fn validate_pending_queue(
    state: &SessionState,
    lane: &LaneName,
    run_id: Option<&RunId>,
    entry_id: &EntryId,
) -> Result<(), SessionError> {
    if let Some(run_id) = run_id {
        validate_open_operation(state, lane, run_id)?;
    }
    let pending = state
        .records_in_sequence_order()
        .iter()
        .rev()
        .find_map(|record| {
            if record.lane() != lane {
                return None;
            }
            match record {
                OperationRecord::QueueCancelled {
                    run_id: owner,
                    entry_id: candidate,
                    ..
                } if candidate == entry_id && owner.as_ref() == run_id => Some(false),
                OperationRecord::QueueEnqueued {
                    run_id: owner,
                    target,
                    ..
                } if target.id() == entry_id && owner.as_ref() == run_id => Some(true),
                OperationRecord::Started { .. }
                | OperationRecord::AbortRequested { .. }
                | OperationRecord::Finished { .. }
                | OperationRecord::StepAttempt { .. }
                | OperationRecord::ToolStarted { .. }
                | OperationRecord::QueueEnqueued { .. }
                | OperationRecord::QueueCancelled { .. }
                | OperationRecord::WriteDeferred { .. }
                | OperationRecord::Usage { .. } => None,
            }
        });
    if pending == Some(true) && state.entry(entry_id).is_none() {
        Ok(())
    } else {
        Err(SessionError::new(
            SessionErrorKind::Storage,
            format!("queue entry {entry_id} is not pending for the requested owner"),
        ))
    }
}

fn validate_queue_owner(
    state: &SessionState,
    lane: &LaneName,
    run_id: Option<&RunId>,
    queue: QueueKind,
) -> Result<(), SessionError> {
    if state.lane_leaf(lane).is_none() {
        return Err(SessionError::new(
            SessionErrorKind::InvalidLane,
            format!("lane {lane} does not exist"),
        ));
    }
    match queue {
        QueueKind::NextRun if run_id.is_none() => Ok(()),
        QueueKind::Steer | QueueKind::FollowUp => {
            let Some(run_id) = run_id else {
                return Err(SessionError::new(
                    SessionErrorKind::Storage,
                    format!("{queue:?} queue requires an active operation"),
                ));
            };
            validate_open_operation(state, lane, run_id)?;
            if operation_has_abort(state, lane, run_id) {
                return Err(SessionError::new(
                    SessionErrorKind::Storage,
                    format!("operation {run_id} no longer accepts queued input after abort"),
                ));
            }
            Ok(())
        }
        QueueKind::NextRun => Err(SessionError::new(
            SessionErrorKind::Storage,
            "next-run queue records cannot belong to an active operation",
        )),
    }
}

fn sequence_overflow() -> SessionError {
    SessionError::new(
        SessionErrorKind::Corruption,
        "session mutation sequence overflowed",
    )
}

/// Extracts the run identity encoded by an operation-start record.
pub(crate) fn started_run_id(record: &OperationRecord) -> Option<RunId> {
    match record {
        OperationRecord::Started { base, .. } => Some(RunId::new(base.id.as_str())),
        _ => None,
    }
}

/// Returns the next one-based attempt in the current structural step series.
///
/// Pinned Pi starts a new series at one when the latest attempt belongs to a
/// different step or its provisioned result entry has already been committed.
/// Only a same-step attempt whose result is still missing is retried with the
/// next number.
pub(crate) fn next_step_attempt(state: &SessionState, run_id: &RunId, step: OperationStep) -> u32 {
    let previous = state
        .records_in_sequence_order()
        .iter()
        .rev()
        .find_map(|record| match record {
            OperationRecord::StepAttempt {
                run_id: candidate,
                step: previous_step,
                attempt,
                result_entry_id,
                ..
            } if candidate == run_id => Some((*previous_step, *attempt, result_entry_id)),
            _ => None,
        });
    match previous {
        Some((previous_step, attempt, result_entry_id))
            if previous_step == step && state.entry(result_entry_id).is_none() =>
        {
            attempt.saturating_add(1)
        }
        Some(_) | None => 1,
    }
}

/// Returns the timestamp used for a synthetic context message derived from an entry.
pub(crate) fn entry_timestamp(entry: &SessionEntry) -> Timestamp {
    entry.base().timestamp
}
