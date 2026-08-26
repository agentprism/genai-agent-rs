//! Durable harness context reconstruction and compaction preparation flow.

use crate::{
    CompactionDecision, CompactionDecisionInput, CompactionError, CompactionInput,
    CompactionPolicy, CompactionResult, LocalCompactionPolicy, LocalSession, Session,
    branch_summary_record, compaction_summary_record, entry_timestamp,
    estimate_harness_context_tokens, is_projectable_harness_role, latest_compaction_summary,
    next_step_attempt, project_record_to_message, started_run_id,
};
use pi_agent_core::{
    AgentRecord, AgentStateView, ContextError, ContextPolicy, LocalContextPolicy,
    PreparedAgentRecords,
};
use pi_agent_session::{
    CompactionReason, EntryId, OperationIntent, OperationOutcome, OperationRecord, OperationStep,
    SessionEntry,
};
use pi_ai::{
    CancellationToken, LocalBoxFuture, Message, ModelRef, ReasoningLevel, RunId, SendBoxFuture,
    Timestamp,
};
use std::{rc::Rc, sync::Arc};

/// Model-visible prefix for a durable compaction summary.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Model-visible suffix for a durable compaction summary.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Model-visible prefix for a durable abandoned-branch summary.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";

/// Model-visible suffix for a durable abandoned-branch summary.
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Context reconstructed from one durable session branch.
#[derive(Clone, Debug)]
pub struct ReconstructedBranchContext {
    /// Model-visible and custom records after applying the latest compaction boundary.
    pub records: Vec<AgentRecord>,
    /// Model selected by the latest branch state change or assistant response.
    pub model: Option<ModelRef>,
    /// Reasoning level selected by the latest branch state change.
    pub reasoning: ReasoningLevel,
    /// Presence-aware durable reasoning override, including explicit `Off`.
    pub reasoning_override: Option<ReasoningLevel>,
    /// Active tool names selected by the latest branch state change.
    pub active_tool_names: Option<Vec<String>>,
}

/// Result of one explicit or automatic context preparation.
#[derive(Clone, Debug)]
pub struct HarnessPreparation {
    /// Base-policy result after durable compaction and branch reconstruction.
    pub prepared: PreparedAgentRecords,
    /// Newly committed compaction entry, when preparation compacted.
    pub compaction_entry_id: Option<EntryId>,
}

/// Explicit Send-capable projection seam for application-defined session entries.
///
/// Pinned Pi omits custom session entries unless a projector registered for
/// their `custom_type` returns context messages. Implementations should return
/// model-visible [`AgentRecord::Llm`] values when custom data belongs in a
/// normal assistant request. Compaction preparation intentionally does not run
/// this projector.
pub trait CustomSessionEntryProjector: Send + Sync + 'static {
    /// Projects one custom entry in the context of its branch path.
    fn project(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError>;
}

/// Local-executor counterpart of [`CustomSessionEntryProjector`].
pub trait LocalCustomSessionEntryProjector: 'static {
    /// Projects one custom entry without requiring thread-safe host state.
    fn project(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError>;
}

/// Pi-compatible default that omits unregistered custom session entries.
#[derive(Clone, Copy, Debug, Default)]
pub struct OmitCustomSessionEntries;

impl CustomSessionEntryProjector for OmitCustomSessionEntries {
    fn project(
        &self,
        _entry: &SessionEntry,
        _index: usize,
        _path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        Ok(Vec::new())
    }
}

impl LocalCustomSessionEntryProjector for OmitCustomSessionEntries {
    fn project(
        &self,
        _entry: &SessionEntry,
        _index: usize,
        _path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        Ok(Vec::new())
    }
}

/// Send harness context policy from Architecture v2 part 2 §7.7.
pub struct HarnessContextPolicy {
    /// Final agent-record transformation delegated after durable preparation.
    pub base: Arc<dyn ContextPolicy>,
    /// Threshold/manual/overflow compaction behavior.
    pub compaction: Arc<dyn CompactionPolicy>,
    /// Selected durable session lane.
    pub session: Arc<Session>,
    /// Explicit projection for application-defined durable entries.
    pub custom_entry_projector: Arc<dyn CustomSessionEntryProjector>,
}

impl HarnessContextPolicy {
    /// Runs an explicit manual compaction and then delegates to the base policy.
    pub async fn compact_manual(
        &self,
        state: AgentStateView<'_>,
        custom_instructions: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        self.prepare_impl(
            state,
            Some(CompactionReason::Manual),
            custom_instructions,
            None,
            cancellation,
        )
        .await
    }

    /// Resumes one durable incomplete compaction operation.
    pub async fn resume_compaction(
        &self,
        state: AgentStateView<'_>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        let session_state = self.session.load_state().await?;
        let open = session_state.open_operations(self.session.lane());
        let operation = match open.as_slice() {
            [operation] => (*operation).clone(),
            [] => {
                return Err(CompactionError::NotResumable {
                    message: "the selected lane has no open operation".to_owned(),
                });
            }
            _ => {
                return Err(CompactionError::NotResumable {
                    message: "the selected lane has multiple open operations".to_owned(),
                });
            }
        };
        let run_id = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
            message: "the open record is not an operation start".to_owned(),
        })?;
        let resume = resume_compaction_spec(&operation, &session_state, &run_id)?;
        if let ResumeCompactionSpec::Committed { result_entry_id } = resume {
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            let mut prepared = self
                .prepare_impl(state, None, None, None, cancellation)
                .await?;
            prepared.compaction_entry_id = Some(result_entry_id);
            return Ok(prepared);
        }
        let ResumeCompactionSpec::Generate {
            reason,
            result_entry_id,
            custom_instructions,
            owns_operation,
        } = resume
        else {
            unreachable!("committed compaction recovery returned above")
        };
        self.prepare_impl(
            state,
            Some(reason),
            custom_instructions,
            Some(ResumeCompaction {
                run_id,
                result_entry_id,
                owns_operation,
            }),
            cancellation,
        )
        .await
    }

    pub(crate) async fn compact_for_operation(
        &self,
        state: AgentStateView<'_>,
        run_id: pi_ai::RunId,
        reason: CompactionReason,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        validate_run_compaction_owner(self.session.as_ref(), &run_id).await?;
        self.prepare_impl(
            state,
            Some(reason),
            None,
            Some(ResumeCompaction {
                run_id,
                result_entry_id: self.session.next_entry_id("compaction"),
                owns_operation: false,
            }),
            cancellation,
        )
        .await
    }

    async fn prepare_impl(
        &self,
        state: AgentStateView<'_>,
        requested_reason: Option<CompactionReason>,
        custom_instructions: Option<String>,
        resume: Option<ResumeCompaction>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        cancellation
            .check()
            .map_err(|_| CompactionError::Cancelled)?;
        let path = self.session.branch_entries().await?;
        let compaction_context = reconstruct_branch_context(&path)?;
        let compactable = compactable_records(&path);
        let context_tokens = estimate_harness_context_tokens(&compaction_context.records)?.tokens;
        let selected_model = compaction_context.model.as_ref().unwrap_or(state.model);
        let decision = if path.is_empty()
            || matches!(path.last(), Some(SessionEntry::Compaction { .. }))
        {
            CompactionDecision::NoCompaction
        } else {
            self.compaction.decide(CompactionDecisionInput {
                records: &compactable.records,
                structural_branch_summary_indices: &compactable.structural_branch_summary_indices,
                context_tokens,
                context_window: 0,
                requested_reason,
                current_model: selected_model,
            })?
        };
        let mut compaction_entry_id = None;
        if let CompactionDecision::Compact {
            reason,
            retained_tail_start,
            summary_model,
        } = decision
        {
            let result_entry_id = resume.as_ref().map_or_else(
                || self.session.next_entry_id("compaction"),
                |value| value.result_entry_id.clone(),
            );
            let (run_id, owns_operation) = if let Some(resume) = resume {
                (resume.run_id, resume.owns_operation)
            } else {
                acquire_compaction_operation(
                    self.session.as_ref(),
                    reason,
                    result_entry_id.clone(),
                    custom_instructions.clone(),
                )
                .await?
            };
            let attempt = next_step_attempt(
                &self.session.load_state().await?,
                &run_id,
                OperationStep::Compaction,
            );
            self.session
                .append_step_attempt(
                    run_id.clone(),
                    OperationStep::Compaction,
                    attempt,
                    result_entry_id.clone(),
                    Some(reason),
                )
                .await?;
            let result = self
                .compaction
                .compact(
                    CompactionInput {
                        records: compactable.records,
                        structural_branch_summary_indices: compactable
                            .structural_branch_summary_indices,
                        retained_tail_start,
                        tokens_before: context_tokens,
                        reason,
                        summary_model,
                        result_entry_id: result_entry_id.clone(),
                        previous_summary: latest_compaction_summary(&path),
                        previous_details: latest_compaction_details(&path),
                        custom_instructions,
                        reasoning: compaction_context
                            .reasoning_override
                            .unwrap_or(state.reasoning),
                        timestamp: path.last().map_or(Timestamp::default(), entry_timestamp),
                    },
                    cancellation.clone(),
                )
                .await?;
            commit_compaction_result(
                self.session.as_ref(),
                run_id.clone(),
                attempt,
                result_entry_id.clone(),
                result,
            )
            .await?;
            if owns_operation {
                self.session
                    .finish_operation(run_id, OperationOutcome::Completed, None)
                    .await?;
            }
            compaction_entry_id = Some(result_entry_id);
        }
        let final_context = if compaction_entry_id.is_some() {
            let final_path = self.session.branch_entries().await?;
            reconstruct_branch_context_send(&final_path, self.custom_entry_projector.as_ref())?
        } else {
            reconstruct_branch_context_send(&path, self.custom_entry_projector.as_ref())?
        };
        let final_model = final_context.model.as_ref().unwrap_or(state.model);
        let final_reasoning = final_context.reasoning_override.unwrap_or(state.reasoning);
        let final_options = options_with_reasoning(state.options, final_reasoning);
        let base_prepared = self
            .base
            .prepare_agent_records(
                AgentStateView {
                    state: state.state,
                    records: &final_context.records,
                    tools: state.tools,
                    model: final_model,
                    reasoning: final_reasoning,
                    options: &final_options,
                },
                cancellation,
            )
            .await
            .map_err(|error| CompactionError::decision(error.to_string()))?;
        let base_prepared = project_known_harness_records(base_prepared);
        Ok(HarnessPreparation {
            prepared: apply_session_overrides(
                base_prepared,
                state.model,
                &final_options,
                &final_context,
            ),
            compaction_entry_id,
        })
    }
}

impl ContextPolicy for HarnessContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            self.prepare_impl(state, None, None, None, cancellation)
                .await
                .map(|result| result.prepared)
                .map_err(|error| ContextError::Projection {
                    message: error.to_string(),
                })
        })
    }
}

/// Local-executor harness context policy.
///
/// Durable storage, base policy, and summary policy may all retain `Rc` state.
pub struct LocalHarnessContextPolicy {
    /// Final local agent-record transformation.
    pub base: Rc<dyn LocalContextPolicy>,
    /// Local threshold/manual/overflow compaction behavior.
    pub compaction: Rc<dyn LocalCompactionPolicy>,
    /// Selected durable session lane.
    pub session: Rc<LocalSession>,
    /// Explicit local projection for application-defined durable entries.
    pub custom_entry_projector: Rc<dyn LocalCustomSessionEntryProjector>,
}

impl LocalHarnessContextPolicy {
    /// Runs an explicit local manual compaction and delegates to the local base policy.
    pub async fn compact_manual(
        &self,
        state: AgentStateView<'_>,
        custom_instructions: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        self.prepare_impl(
            state,
            Some(CompactionReason::Manual),
            custom_instructions,
            None,
            cancellation,
        )
        .await
    }

    /// Resumes one durable incomplete local compaction operation.
    pub async fn resume_compaction(
        &self,
        state: AgentStateView<'_>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        let session_state = self.session.load_state().await?;
        let open = session_state.open_operations(self.session.lane());
        let operation = match open.as_slice() {
            [operation] => (*operation).clone(),
            [] => {
                return Err(CompactionError::NotResumable {
                    message: "the selected lane has no open operation".to_owned(),
                });
            }
            _ => {
                return Err(CompactionError::NotResumable {
                    message: "the selected lane has multiple open operations".to_owned(),
                });
            }
        };
        let run_id = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
            message: "the open record is not an operation start".to_owned(),
        })?;
        let resume = resume_compaction_spec(&operation, &session_state, &run_id)?;
        if let ResumeCompactionSpec::Committed { result_entry_id } = resume {
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            let mut prepared = self
                .prepare_impl(state, None, None, None, cancellation)
                .await?;
            prepared.compaction_entry_id = Some(result_entry_id);
            return Ok(prepared);
        }
        let ResumeCompactionSpec::Generate {
            reason,
            result_entry_id,
            custom_instructions,
            owns_operation,
        } = resume
        else {
            unreachable!("committed compaction recovery returned above")
        };
        self.prepare_impl(
            state,
            Some(reason),
            custom_instructions,
            Some(ResumeCompaction {
                run_id,
                result_entry_id,
                owns_operation,
            }),
            cancellation,
        )
        .await
    }

    pub(crate) async fn compact_for_operation(
        &self,
        state: AgentStateView<'_>,
        run_id: RunId,
        reason: CompactionReason,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        validate_local_run_compaction_owner(self.session.as_ref(), &run_id).await?;
        self.prepare_impl(
            state,
            Some(reason),
            None,
            Some(ResumeCompaction {
                run_id,
                result_entry_id: self.session.next_entry_id("compaction"),
                owns_operation: false,
            }),
            cancellation,
        )
        .await
    }

    async fn prepare_impl(
        &self,
        state: AgentStateView<'_>,
        requested_reason: Option<CompactionReason>,
        custom_instructions: Option<String>,
        resume: Option<ResumeCompaction>,
        cancellation: CancellationToken,
    ) -> Result<HarnessPreparation, CompactionError> {
        cancellation
            .check()
            .map_err(|_| CompactionError::Cancelled)?;
        let path = self.session.branch_entries().await?;
        let compaction_context = reconstruct_branch_context(&path)?;
        let compactable = compactable_records(&path);
        let context_tokens = estimate_harness_context_tokens(&compaction_context.records)?.tokens;
        let selected_model = compaction_context.model.as_ref().unwrap_or(state.model);
        let decision = if path.is_empty()
            || matches!(path.last(), Some(SessionEntry::Compaction { .. }))
        {
            CompactionDecision::NoCompaction
        } else {
            self.compaction.decide(CompactionDecisionInput {
                records: &compactable.records,
                structural_branch_summary_indices: &compactable.structural_branch_summary_indices,
                context_tokens,
                context_window: 0,
                requested_reason,
                current_model: selected_model,
            })?
        };
        let mut compaction_entry_id = None;
        if let CompactionDecision::Compact {
            reason,
            retained_tail_start,
            summary_model,
        } = decision
        {
            let result_entry_id = resume.as_ref().map_or_else(
                || self.session.next_entry_id("compaction"),
                |value| value.result_entry_id.clone(),
            );
            let (run_id, owns_operation) = if let Some(resume) = resume {
                (resume.run_id, resume.owns_operation)
            } else {
                acquire_local_compaction_operation(
                    self.session.as_ref(),
                    reason,
                    result_entry_id.clone(),
                    custom_instructions.clone(),
                )
                .await?
            };
            let attempt = next_step_attempt(
                &self.session.load_state().await?,
                &run_id,
                OperationStep::Compaction,
            );
            self.session
                .append_step_attempt(
                    run_id.clone(),
                    OperationStep::Compaction,
                    attempt,
                    result_entry_id.clone(),
                    Some(reason),
                )
                .await?;
            let result = self
                .compaction
                .compact(
                    CompactionInput {
                        records: compactable.records,
                        structural_branch_summary_indices: compactable
                            .structural_branch_summary_indices,
                        retained_tail_start,
                        tokens_before: context_tokens,
                        reason,
                        summary_model,
                        result_entry_id: result_entry_id.clone(),
                        previous_summary: latest_compaction_summary(&path),
                        previous_details: latest_compaction_details(&path),
                        custom_instructions,
                        reasoning: compaction_context
                            .reasoning_override
                            .unwrap_or(state.reasoning),
                        timestamp: path.last().map_or(Timestamp::default(), entry_timestamp),
                    },
                    cancellation.clone(),
                )
                .await?;
            commit_local_compaction_result(
                self.session.as_ref(),
                run_id.clone(),
                attempt,
                result_entry_id.clone(),
                result,
            )
            .await?;
            if owns_operation {
                self.session
                    .finish_operation(run_id, OperationOutcome::Completed, None)
                    .await?;
            }
            compaction_entry_id = Some(result_entry_id);
        }
        let final_context = if compaction_entry_id.is_some() {
            let final_path = self.session.branch_entries().await?;
            reconstruct_branch_context_local(&final_path, self.custom_entry_projector.as_ref())?
        } else {
            reconstruct_branch_context_local(&path, self.custom_entry_projector.as_ref())?
        };
        let final_model = final_context.model.as_ref().unwrap_or(state.model);
        let final_reasoning = final_context.reasoning_override.unwrap_or(state.reasoning);
        let final_options = options_with_reasoning(state.options, final_reasoning);
        let prepared = self
            .base
            .prepare_agent_records(
                AgentStateView {
                    state: state.state,
                    records: &final_context.records,
                    tools: state.tools,
                    model: final_model,
                    reasoning: final_reasoning,
                    options: &final_options,
                },
                cancellation,
            )
            .await
            .map_err(|error| CompactionError::decision(error.to_string()))?;
        let prepared = project_known_harness_records(prepared);
        Ok(HarnessPreparation {
            prepared: apply_session_overrides(
                prepared,
                state.model,
                &final_options,
                &final_context,
            ),
            compaction_entry_id,
        })
    }
}

impl LocalContextPolicy for LocalHarnessContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            self.prepare_impl(state, None, None, None, cancellation)
                .await
                .map(|result| result.prepared)
                .map_err(|error| ContextError::Projection {
                    message: error.to_string(),
                })
        })
    }
}

#[derive(Clone, Debug)]
struct ResumeCompaction {
    run_id: pi_ai::RunId,
    result_entry_id: EntryId,
    owns_operation: bool,
}

#[derive(Clone, Debug)]
enum ResumeCompactionSpec {
    Committed {
        result_entry_id: EntryId,
    },
    Generate {
        reason: CompactionReason,
        result_entry_id: EntryId,
        custom_instructions: Option<String>,
        owns_operation: bool,
    },
}

async fn acquire_compaction_operation(
    session: &Session,
    reason: CompactionReason,
    result_entry_id: EntryId,
    custom_instructions: Option<String>,
) -> Result<(RunId, bool), CompactionError> {
    let open = session.open_operation().await?;
    if reason == CompactionReason::Manual {
        if open.is_some() {
            return Err(CompactionError::NotResumable {
                message: "manual compaction requires an idle session lane".to_owned(),
            });
        }
    } else if let Some(operation) = open {
        let OperationRecord::Started {
            intent: OperationIntent::Run { .. },
            ..
        } = &operation
        else {
            return Err(CompactionError::NotResumable {
                message: "automatic compaction can reuse only an open run operation".to_owned(),
            });
        };
        let run_id = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
            message: "open run operation has no run identity".to_owned(),
        })?;
        return Ok((run_id, false));
    }

    let run_id = session
        .start_operation(OperationIntent::Compaction {
            custom_instructions,
            result_entry_id,
        })
        .await?;
    Ok((run_id, true))
}

async fn acquire_local_compaction_operation(
    session: &LocalSession,
    reason: CompactionReason,
    result_entry_id: EntryId,
    custom_instructions: Option<String>,
) -> Result<(RunId, bool), CompactionError> {
    let open = session.open_operation().await?;
    if reason == CompactionReason::Manual {
        if open.is_some() {
            return Err(CompactionError::NotResumable {
                message: "manual compaction requires an idle session lane".to_owned(),
            });
        }
    } else if let Some(operation) = open {
        let OperationRecord::Started {
            intent: OperationIntent::Run { .. },
            ..
        } = &operation
        else {
            return Err(CompactionError::NotResumable {
                message: "automatic compaction can reuse only an open run operation".to_owned(),
            });
        };
        let run_id = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
            message: "open run operation has no run identity".to_owned(),
        })?;
        return Ok((run_id, false));
    }

    let run_id = session
        .start_operation(OperationIntent::Compaction {
            custom_instructions,
            result_entry_id,
        })
        .await?;
    Ok((run_id, true))
}

async fn validate_run_compaction_owner(
    session: &Session,
    expected_run_id: &RunId,
) -> Result<(), CompactionError> {
    let operation =
        session
            .open_operation()
            .await?
            .ok_or_else(|| CompactionError::NotResumable {
                message: "compaction run owner is no longer open".to_owned(),
            })?;
    let OperationRecord::Started {
        intent: OperationIntent::Run { .. },
        ..
    } = &operation
    else {
        return Err(CompactionError::NotResumable {
            message: "compaction can borrow only an open run operation".to_owned(),
        });
    };
    let observed = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
        message: "open run operation has no run identity".to_owned(),
    })?;
    if observed != *expected_run_id {
        return Err(CompactionError::NotResumable {
            message: format!(
                "compaction owner mismatch: expected {expected_run_id}, found {observed}"
            ),
        });
    }
    Ok(())
}

async fn validate_local_run_compaction_owner(
    session: &LocalSession,
    expected_run_id: &RunId,
) -> Result<(), CompactionError> {
    let operation =
        session
            .open_operation()
            .await?
            .ok_or_else(|| CompactionError::NotResumable {
                message: "compaction run owner is no longer open".to_owned(),
            })?;
    let OperationRecord::Started {
        intent: OperationIntent::Run { .. },
        ..
    } = &operation
    else {
        return Err(CompactionError::NotResumable {
            message: "compaction can borrow only an open run operation".to_owned(),
        });
    };
    let observed = started_run_id(&operation).ok_or_else(|| CompactionError::NotResumable {
        message: "open run operation has no run identity".to_owned(),
    })?;
    if observed != *expected_run_id {
        return Err(CompactionError::NotResumable {
            message: format!(
                "compaction owner mismatch: expected {expected_run_id}, found {observed}"
            ),
        });
    }
    Ok(())
}

async fn commit_compaction_result(
    session: &Session,
    run_id: pi_ai::RunId,
    attempt: u32,
    entry_id: EntryId,
    result: CompactionResult,
) -> Result<(), CompactionError> {
    session
        .commit_compaction(
            run_id,
            attempt,
            entry_id,
            result.summary,
            result.retained_tail,
            result.tokens_before,
            result.details,
            result.usage,
            result.cost,
            result.stop_reason,
        )
        .await?;
    Ok(())
}

async fn commit_local_compaction_result(
    session: &LocalSession,
    run_id: pi_ai::RunId,
    attempt: u32,
    entry_id: EntryId,
    result: CompactionResult,
) -> Result<(), CompactionError> {
    session
        .commit_compaction(
            run_id,
            attempt,
            entry_id,
            result.summary,
            result.retained_tail,
            result.tokens_before,
            result.details,
            result.usage,
            result.cost,
            result.stop_reason,
        )
        .await?;
    Ok(())
}

fn resume_compaction_spec(
    operation: &OperationRecord,
    state: &pi_agent_session::SessionState,
    run_id: &pi_ai::RunId,
) -> Result<ResumeCompactionSpec, CompactionError> {
    let OperationRecord::Started { intent, .. } = operation else {
        return Err(CompactionError::NotResumable {
            message: "open record is not an operation start".to_owned(),
        });
    };
    let latest_attempt = state
        .records_in_sequence_order()
        .iter()
        .rev()
        .find_map(|record| match record {
            OperationRecord::StepAttempt {
                run_id: candidate,
                step,
                result_entry_id,
                compaction_reason,
                ..
            } if candidate == run_id => Some((*step, *compaction_reason, result_entry_id.clone())),
            _ => None,
        });
    match intent {
        OperationIntent::Compaction {
            custom_instructions: _,
            result_entry_id,
        } if state.entry(result_entry_id).is_some() => Ok(ResumeCompactionSpec::Committed {
            result_entry_id: result_entry_id.clone(),
        }),
        OperationIntent::Compaction {
            custom_instructions,
            result_entry_id,
        } => {
            let attempt = latest_attempt.filter(|(step, _, _)| *step == OperationStep::Compaction);
            Ok(ResumeCompactionSpec::Generate {
                reason: attempt
                    .as_ref()
                    .and_then(|value| value.1)
                    .unwrap_or(CompactionReason::Manual),
                result_entry_id: attempt.map_or_else(
                    || result_entry_id.clone(),
                    |(_, _, attempted_result)| attempted_result,
                ),
                custom_instructions: custom_instructions.clone(),
                owns_operation: true,
            })
        }
        OperationIntent::Run { .. }
            if matches!(
                latest_attempt,
                Some((OperationStep::Compaction, Some(_), ref result_entry_id))
                    if state.entry(result_entry_id).is_none()
            ) =>
        {
            let Some((_, Some(reason), result_entry_id)) = latest_attempt else {
                unreachable!("guard requires an incomplete compaction attempt")
            };
            Ok(ResumeCompactionSpec::Generate {
                reason,
                result_entry_id,
                custom_instructions: None,
                owns_operation: false,
            })
        }
        _ => Err(CompactionError::NotResumable {
            message: "open operation has no incomplete compaction step".to_owned(),
        }),
    }
}

struct CompactableRecords {
    records: Vec<AgentRecord>,
    structural_branch_summary_indices: Vec<usize>,
}

fn compactable_records(path: &[SessionEntry]) -> CompactableRecords {
    let latest = path
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }));
    let mut records = Vec::new();
    let start = if let Some(index) = latest {
        if let SessionEntry::Compaction { retained_tail, .. } = &path[index] {
            records.extend(retained_tail.clone());
        }
        index.saturating_add(1)
    } else {
        0
    };
    let mut structural_branch_summary_indices = Vec::new();
    for entry in path.iter().skip(start) {
        if let SessionEntry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } = entry
        {
            structural_branch_summary_indices.push(records.len());
            records.push(branch_summary_record(summary, from_id, base.timestamp));
        } else {
            append_entry_records(entry, &mut records, false);
        }
    }
    CompactableRecords {
        records,
        structural_branch_summary_indices,
    }
}

fn latest_compaction_details(path: &[SessionEntry]) -> Option<pi_ai::VersionedExtension> {
    path.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction { details, .. } => details.clone(),
        _ => None,
    })
}

/// Applies the latest compaction boundary and materializes durable summary entries.
pub fn reconstruct_branch_context(
    path: &[SessionEntry],
) -> Result<ReconstructedBranchContext, CompactionError> {
    reconstruct_branch_context_with(path, |_entry, _index, _entries| Ok(Vec::new()))
}

fn reconstruct_branch_context_send(
    path: &[SessionEntry],
    projector: &dyn CustomSessionEntryProjector,
) -> Result<ReconstructedBranchContext, CompactionError> {
    reconstruct_branch_context_with(path, |entry, index, entries| {
        projector.project(entry, index, entries)
    })
}

fn reconstruct_branch_context_local(
    path: &[SessionEntry],
    projector: &dyn LocalCustomSessionEntryProjector,
) -> Result<ReconstructedBranchContext, CompactionError> {
    reconstruct_branch_context_with(path, |entry, index, entries| {
        projector.project(entry, index, entries)
    })
}

fn reconstruct_branch_context_with(
    path: &[SessionEntry],
    mut project_custom: impl FnMut(
        &SessionEntry,
        usize,
        &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError>,
) -> Result<ReconstructedBranchContext, CompactionError> {
    let mut model = None;
    let mut reasoning = ReasoningLevel::Off;
    let mut reasoning_override = None;
    let mut active_tool_names = None;
    for entry in path {
        match entry {
            SessionEntry::ModelChange { model: changed, .. } => model = Some(changed.clone()),
            SessionEntry::ReasoningChange { level, .. } => {
                reasoning = *level;
                reasoning_override = Some(*level);
            }
            SessionEntry::ActiveToolsChange { tool_names, .. } => {
                active_tool_names = Some(tool_names.clone());
            }
            SessionEntry::Message {
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } => {
                model = Some(ModelRef {
                    provider: assistant.provider.clone(),
                    model: assistant.requested_model.clone(),
                });
            }
            SessionEntry::Message { .. }
            | SessionEntry::Compaction { .. }
            | SessionEntry::BranchSummary { .. }
            | SessionEntry::Custom { .. } => {}
        }
    }
    let latest = path
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }));
    let start = latest.unwrap_or(0);
    let context_entries = &path[start..];
    let mut records = Vec::new();
    for (index, entry) in context_entries.iter().enumerate() {
        append_entry_records(entry, &mut records, true);
        if matches!(entry, SessionEntry::Custom { .. }) {
            records.extend(project_custom(entry, index, context_entries)?);
        }
    }
    Ok(ReconstructedBranchContext {
        records,
        model,
        reasoning,
        reasoning_override,
        active_tool_names,
    })
}

fn append_entry_records(
    entry: &SessionEntry,
    records: &mut Vec<AgentRecord>,
    include_compaction: bool,
) {
    match entry {
        SessionEntry::Message {
            message: AgentRecord::Llm(Message::Assistant(assistant)),
            ..
        } if include_compaction
            && assistant.finish.reason == pi_ai::AssistantFinishReason::Deferred => {}
        SessionEntry::Message { message, .. } => records.push(message.clone()),
        SessionEntry::Compaction {
            base,
            summary,
            retained_tail,
            tokens_before,
            ..
        } if include_compaction => {
            records.push(compaction_summary_record(
                summary,
                *tokens_before,
                base.timestamp,
            ));
            records.extend(retained_tail.clone());
        }
        SessionEntry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } if !summary.is_empty() => {
            records.push(branch_summary_record(summary, from_id, base.timestamp))
        }
        SessionEntry::ModelChange { .. }
        | SessionEntry::ReasoningChange { .. }
        | SessionEntry::ActiveToolsChange { .. }
        | SessionEntry::Compaction { .. }
        | SessionEntry::BranchSummary { .. }
        | SessionEntry::Custom { .. } => {}
    }
}

fn project_known_harness_records(mut prepared: PreparedAgentRecords) -> PreparedAgentRecords {
    prepared.records = prepared
        .records
        .into_iter()
        .enumerate()
        .filter_map(|(index, record)| match &record {
            AgentRecord::Custom { type_name, .. } if is_projectable_harness_role(type_name) => {
                project_record_to_message(&record, index).map(AgentRecord::Llm)
            }
            AgentRecord::Llm(_) | AgentRecord::Custom { .. } => Some(record),
        })
        .collect();
    prepared
}

fn apply_session_overrides(
    mut prepared: PreparedAgentRecords,
    original_model: &ModelRef,
    reconstructed_options: &pi_ai::SimpleGenerationOptions,
    context: &ReconstructedBranchContext,
) -> PreparedAgentRecords {
    if prepared.model_override.is_none()
        && let Some(model) = &context.model
        && model != original_model
    {
        prepared.model_override = Some(model.clone());
    }
    // `options_override` is a complete replacement, so a base-policy value is
    // authoritative. Synthesize the durable reasoning replacement only for a
    // base policy that deliberately returned no option projection.
    if prepared.options_override.is_none() && context.reasoning_override.is_some() {
        prepared.options_override = Some(reconstructed_options.clone());
    }
    prepared
}

fn options_with_reasoning(
    configured: &pi_ai::SimpleGenerationOptions,
    reasoning: ReasoningLevel,
) -> pi_ai::SimpleGenerationOptions {
    let mut reconstructed = configured.clone();
    reconstructed.reasoning = (reasoning != ReasoningLevel::Off).then_some(reasoning);
    reconstructed
}
