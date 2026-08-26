//! Harness-owned context-overflow compaction and assistant retry.

use crate::{HarnessContextPolicy, HarnessOperationError, LocalSession, Session};
use pi_agent_core::{AgentRecord, AgentStateView, ContextPolicy, PreparedAgentRecords};
use pi_agent_session::{
    CompactionReason, EntryId, OperationIntent, OperationOutcome, OperationStep, ProvisionedEntry,
    UsageAttribution,
};
use pi_ai::{
    AssistantFinishReason, AssistantMessage, CancellationToken, LocalBoxFuture, PublicError, RunId,
    SendBoxFuture, is_context_overflow,
};
use std::{rc::Rc, sync::Arc};

/// Caller intent persisted before a harness-owned assistant operation begins.
#[derive(Clone, Debug, Default)]
pub struct OverflowRunIntent {
    /// Normalized original prompt.
    pub original_prompt: Vec<AgentRecord>,
    /// Provisioned initial entries already selected by the caller.
    pub initial_messages: Vec<ProvisionedEntry>,
    /// Optional operation-scoped system-prompt replacement.
    pub system_prompt_override: Option<String>,
}

/// Owned assistant-attempt input supplied by overflow orchestration.
#[derive(Clone, Debug)]
pub struct AssistantStepInput {
    /// Fully prepared records after durable branch reconstruction and compaction.
    pub prepared: PreparedAgentRecords,
    /// Preallocated durable result entry for this assistant request.
    pub result_entry_id: EntryId,
    /// One-based attempt for this result entry. Overflow recovery uses a new
    /// result entry and therefore restarts this counter at one, matching Pi.
    pub attempt: u32,
}

/// Object-safe assistant request seam used by harness overflow orchestration.
pub trait AssistantStep: Send + Sync + 'static {
    /// Executes one assistant request and returns its terminal in-band message.
    fn execute(
        &self,
        input: AssistantStepInput,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>>;
}

/// Local-executor assistant request seam used by overflow orchestration.
pub trait LocalAssistantStep: 'static {
    /// Executes one local assistant request and returns its terminal in-band message.
    fn execute(
        &self,
        input: AssistantStepInput,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>>;
}

/// Terminal result of one same-operation overflow-retry sequence.
#[derive(Clone, Debug)]
pub struct OverflowRetryResult {
    /// Durable harness operation identity shared by every attempt and compaction.
    pub run_id: RunId,
    /// Committed terminal assistant entry.
    pub assistant_entry_id: EntryId,
    /// Terminal assistant message.
    pub message: AssistantMessage,
    /// Whether one overflow compaction and retry occurred.
    pub recovered_from_overflow: bool,
}

/// Executes one assistant step with at most one harness-owned overflow recovery.
pub struct OverflowRetryExecutor {
    /// Durable context policy used before both assistant attempts.
    pub context_policy: Arc<HarnessContextPolicy>,
    /// Selected durable session lane.
    pub session: Arc<Session>,
    /// Current model context window used by Pi's overflow classifier.
    pub context_window: Option<u64>,
}

impl OverflowRetryExecutor {
    /// Starts and completes one durable run operation.
    ///
    /// Provider transport retries remain inside API implementations. This
    /// method performs only the one logical assistant retry authorized by
    /// Architecture v2 part 2 §7.7 after classified context overflow.
    pub async fn run(
        &self,
        state: AgentStateView<'_>,
        intent: OverflowRunIntent,
        step: &dyn AssistantStep,
        cancellation: CancellationToken,
    ) -> Result<OverflowRetryResult, HarnessOperationError> {
        let initial_messages = intent.initial_messages.clone();
        let run_id = self
            .session
            .start_operation(OperationIntent::Run {
                original_prompt: intent.original_prompt,
                initial_messages: intent.initial_messages,
                system_prompt_override: intent.system_prompt_override,
                resume_data: Default::default(),
            })
            .await
            .map_err(crate::CompactionError::from)?;
        self.session
            .commit_provisioned_entries(initial_messages)
            .await
            .map_err(crate::CompactionError::from)?;
        let first = self
            .execute_attempt(
                state.state,
                state.records,
                state.tools,
                state.model,
                state.reasoning,
                state.options,
                run_id.clone(),
                step,
                cancellation.clone(),
            )
            .await?;
        if !is_context_overflow(&first.message, self.context_window) {
            return self.finish(run_id, first, false).await;
        }

        self.session
            .append_usage(
                UsageAttribution::Assistant {
                    run_id: run_id.clone(),
                    entry_id: first.entry_id,
                    attempt: 1,
                    stop_reason: first.message.finish.reason,
                },
                first.message.usage,
                first.message.cost,
            )
            .await
            .map_err(crate::CompactionError::from)?;
        self.context_policy
            .compact_for_operation(
                AgentStateView {
                    state: state.state,
                    records: state.records,
                    tools: state.tools,
                    model: state.model,
                    reasoning: state.reasoning,
                    options: state.options,
                },
                run_id.clone(),
                CompactionReason::Overflow,
                cancellation.clone(),
            )
            .await?;
        let retry = self
            .execute_attempt(
                state.state,
                state.records,
                state.tools,
                state.model,
                state.reasoning,
                state.options,
                run_id.clone(),
                step,
                cancellation,
            )
            .await?;
        if is_context_overflow(&retry.message, self.context_window) {
            self.finish_exhausted(run_id, retry).await?;
            return Err(HarnessOperationError::OverflowRecoveryExhausted);
        }
        self.finish(run_id, retry, true).await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "borrowed AgentStateView fields must survive two awaited attempts"
    )]
    async fn execute_attempt(
        &self,
        state: &pi_agent_core::AgentState,
        records: &[AgentRecord],
        tools: &[pi_ai::ToolSpec],
        model: &pi_ai::ModelRef,
        reasoning: pi_ai::ReasoningLevel,
        options: &pi_ai::SimpleGenerationOptions,
        run_id: RunId,
        step: &dyn AssistantStep,
        cancellation: CancellationToken,
    ) -> Result<ExecutedAttempt, HarnessOperationError> {
        let prepared = self
            .context_policy
            .prepare_agent_records(
                AgentStateView {
                    state,
                    records,
                    tools,
                    model,
                    reasoning,
                    options,
                },
                cancellation.clone(),
            )
            .await
            .map_err(|error| {
                HarnessOperationError::Compaction(crate::CompactionError::decision(
                    error.to_string(),
                ))
            })?;
        let entry_id = self.session.next_entry_id("assistant");
        self.session
            .append_step_attempt(run_id, OperationStep::Assistant, 1, entry_id.clone(), None)
            .await
            .map_err(crate::CompactionError::from)?;
        let message = step
            .execute(
                AssistantStepInput {
                    prepared,
                    result_entry_id: entry_id.clone(),
                    attempt: 1,
                },
                cancellation,
            )
            .await?;
        Ok(ExecutedAttempt { entry_id, message })
    }

    async fn finish(
        &self,
        run_id: RunId,
        attempt: ExecutedAttempt,
        recovered_from_overflow: bool,
    ) -> Result<OverflowRetryResult, HarnessOperationError> {
        self.session
            .commit_assistant(
                run_id.clone(),
                1,
                attempt.entry_id.clone(),
                attempt.message.clone(),
            )
            .await
            .map_err(crate::CompactionError::from)?;
        let (outcome, error) = operation_outcome(&attempt.message);
        self.session
            .finish_operation(run_id.clone(), outcome, error)
            .await
            .map_err(crate::CompactionError::from)?;
        Ok(OverflowRetryResult {
            run_id,
            assistant_entry_id: attempt.entry_id,
            message: attempt.message,
            recovered_from_overflow,
        })
    }

    async fn finish_exhausted(
        &self,
        run_id: RunId,
        attempt: ExecutedAttempt,
    ) -> Result<(), HarnessOperationError> {
        self.session
            .commit_assistant(run_id.clone(), 1, attempt.entry_id, attempt.message.clone())
            .await
            .map_err(crate::CompactionError::from)?;
        self.session
            .finish_operation(
                run_id,
                OperationOutcome::Failed,
                Some(overflow_recovery_exhausted_error(&attempt.message)),
            )
            .await
            .map_err(crate::CompactionError::from)?;
        Ok(())
    }
}

/// Local-executor same-operation overflow retry.
pub struct LocalOverflowRetryExecutor {
    /// Durable local context policy used before both assistant attempts.
    pub context_policy: Rc<crate::LocalHarnessContextPolicy>,
    /// Selected durable session lane.
    pub session: Rc<LocalSession>,
    /// Current model context window used by Pi's overflow classifier.
    pub context_window: Option<u64>,
}

impl LocalOverflowRetryExecutor {
    /// Starts and completes one local durable run operation with one bounded overflow recovery.
    pub async fn run(
        &self,
        state: AgentStateView<'_>,
        intent: OverflowRunIntent,
        step: &dyn LocalAssistantStep,
        cancellation: CancellationToken,
    ) -> Result<OverflowRetryResult, HarnessOperationError> {
        let initial_messages = intent.initial_messages.clone();
        let run_id = self
            .session
            .start_operation(OperationIntent::Run {
                original_prompt: intent.original_prompt,
                initial_messages: intent.initial_messages,
                system_prompt_override: intent.system_prompt_override,
                resume_data: Default::default(),
            })
            .await
            .map_err(crate::CompactionError::from)?;
        self.session
            .commit_provisioned_entries(initial_messages)
            .await
            .map_err(crate::CompactionError::from)?;
        let first = self
            .execute_attempt(
                state.state,
                state.records,
                state.tools,
                state.model,
                state.reasoning,
                state.options,
                run_id.clone(),
                step,
                cancellation.clone(),
            )
            .await?;
        if !is_context_overflow(&first.message, self.context_window) {
            return self.finish(run_id, first, false).await;
        }
        self.session
            .append_usage(
                UsageAttribution::Assistant {
                    run_id: run_id.clone(),
                    entry_id: first.entry_id,
                    attempt: 1,
                    stop_reason: first.message.finish.reason,
                },
                first.message.usage,
                first.message.cost,
            )
            .await
            .map_err(crate::CompactionError::from)?;
        self.context_policy
            .compact_for_operation(
                AgentStateView {
                    state: state.state,
                    records: state.records,
                    tools: state.tools,
                    model: state.model,
                    reasoning: state.reasoning,
                    options: state.options,
                },
                run_id.clone(),
                CompactionReason::Overflow,
                cancellation.clone(),
            )
            .await?;
        let retry = self
            .execute_attempt(
                state.state,
                state.records,
                state.tools,
                state.model,
                state.reasoning,
                state.options,
                run_id.clone(),
                step,
                cancellation,
            )
            .await?;
        if is_context_overflow(&retry.message, self.context_window) {
            self.finish_exhausted(run_id, retry).await?;
            return Err(HarnessOperationError::OverflowRecoveryExhausted);
        }
        self.finish(run_id, retry, true).await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "borrowed AgentStateView fields must survive two awaited attempts"
    )]
    async fn execute_attempt(
        &self,
        state: &pi_agent_core::AgentState,
        records: &[AgentRecord],
        tools: &[pi_ai::ToolSpec],
        model: &pi_ai::ModelRef,
        reasoning: pi_ai::ReasoningLevel,
        options: &pi_ai::SimpleGenerationOptions,
        run_id: RunId,
        step: &dyn LocalAssistantStep,
        cancellation: CancellationToken,
    ) -> Result<ExecutedAttempt, HarnessOperationError> {
        let prepared = pi_agent_core::LocalContextPolicy::prepare_agent_records(
            self.context_policy.as_ref(),
            AgentStateView {
                state,
                records,
                tools,
                model,
                reasoning,
                options,
            },
            cancellation.clone(),
        )
        .await
        .map_err(|error| {
            HarnessOperationError::Compaction(crate::CompactionError::decision(error.to_string()))
        })?;
        let entry_id = self.session.next_entry_id("assistant");
        self.session
            .append_step_attempt(run_id, OperationStep::Assistant, 1, entry_id.clone(), None)
            .await
            .map_err(crate::CompactionError::from)?;
        let message = step
            .execute(
                AssistantStepInput {
                    prepared,
                    result_entry_id: entry_id.clone(),
                    attempt: 1,
                },
                cancellation,
            )
            .await?;
        Ok(ExecutedAttempt { entry_id, message })
    }

    async fn finish(
        &self,
        run_id: RunId,
        attempt: ExecutedAttempt,
        recovered_from_overflow: bool,
    ) -> Result<OverflowRetryResult, HarnessOperationError> {
        self.session
            .commit_assistant(
                run_id.clone(),
                1,
                attempt.entry_id.clone(),
                attempt.message.clone(),
            )
            .await
            .map_err(crate::CompactionError::from)?;
        let (outcome, error) = operation_outcome(&attempt.message);
        self.session
            .finish_operation(run_id.clone(), outcome, error)
            .await
            .map_err(crate::CompactionError::from)?;
        Ok(OverflowRetryResult {
            run_id,
            assistant_entry_id: attempt.entry_id,
            message: attempt.message,
            recovered_from_overflow,
        })
    }

    async fn finish_exhausted(
        &self,
        run_id: RunId,
        attempt: ExecutedAttempt,
    ) -> Result<(), HarnessOperationError> {
        self.session
            .commit_assistant(run_id.clone(), 1, attempt.entry_id, attempt.message.clone())
            .await
            .map_err(crate::CompactionError::from)?;
        self.session
            .finish_operation(
                run_id,
                OperationOutcome::Failed,
                Some(overflow_recovery_exhausted_error(&attempt.message)),
            )
            .await
            .map_err(crate::CompactionError::from)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct ExecutedAttempt {
    entry_id: EntryId,
    message: AssistantMessage,
}

fn operation_outcome(message: &AssistantMessage) -> (OperationOutcome, Option<PublicError>) {
    match message.finish.reason {
        AssistantFinishReason::Aborted => (
            OperationOutcome::Aborted,
            message.finish.error.clone().or_else(|| {
                Some(PublicError {
                    code: "cancelled".to_owned(),
                    message: "assistant request was cancelled".to_owned(),
                    retryable: false,
                    provider_code: None,
                    status: None,
                    request_id: None,
                })
            }),
        ),
        AssistantFinishReason::Error => (OperationOutcome::Failed, message.finish.error.clone()),
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => (OperationOutcome::Completed, None),
    }
}

fn overflow_recovery_exhausted_error(message: &AssistantMessage) -> PublicError {
    let source = message.finish.error.as_ref();
    PublicError {
        code: "context_overflow_recovery_exhausted".to_owned(),
        message: "context overflow persisted after compaction and retry".to_owned(),
        retryable: false,
        provider_code: source.and_then(|error| error.provider_code.clone()),
        status: source.and_then(|error| error.status),
        request_id: source.and_then(|error| error.request_id.clone()),
    }
}
