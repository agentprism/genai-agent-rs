//! Durable branch navigation and abandoned-segment summarization.

use crate::{
    BranchSummaryError, LocalSession, SUMMARIZATION_SYSTEM_PROMPT, Session, branch_summary_record,
    compaction_summary_record, estimate_record_tokens,
    file_operations::{FileOperations, file_operation_details, format_file_operations},
    next_step_attempt, serialize_conversation, started_run_id,
};
use agentprism_ai::{
    AssistantFinishReason, AssistantMessage, CacheRetention, CancellationToken, ContentBlock,
    ContentBlockId, Context, Cost, LocalBoxFuture, LocalModelRuntime, Message, MessageId, ModelRef,
    ModelRequest, ModelRuntime, ReasoningLevel, SendBoxFuture, SimpleGenerationOptions, Timestamp,
    Usage, UserMessage, VersionedExtension,
};
use agentprism_core::AgentRecord;
use agentprism_session::{
    EntryId, OperationIntent, OperationOutcome, OperationRecord, OperationStep, SessionEntry,
    SessionState,
};
use futures_util::StreamExt;
use std::{collections::BTreeSet, rc::Rc, sync::Arc};

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Entries collected from the abandoned side of a navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectedBranchEntries {
    /// Abandoned entries in chronological order, excluding the common ancestor.
    pub entries: Vec<SessionEntry>,
    /// Deepest common ancestor of the old and target leaves.
    pub common_ancestor_id: Option<EntryId>,
    /// Target-side entries after the common ancestor, in chronological order.
    pub target_tail: Vec<SessionEntry>,
}

/// Complete owned branch-summary policy input from Architecture v2 part 2 §7.8.
#[derive(Clone, Debug)]
pub struct BranchSummaryInput {
    /// Deepest common ancestor of the abandoned and target branches.
    pub common_ancestor_id: Option<EntryId>,
    /// Chronological abandoned branch entries.
    pub abandoned_entries: Vec<SessionEntry>,
    /// Chronological target branch tail after the common ancestor.
    pub target_tail: Vec<SessionEntry>,
    /// Optional caller navigation instructions.
    pub custom_instructions: Option<String>,
    /// Whether custom instructions replace rather than extend the default prompt.
    pub replace_instructions: bool,
    /// Active model at the abandoned leaf, when derivable.
    pub active_model: Option<ModelRef>,
    /// Active reasoning level at the abandoned leaf.
    pub reasoning: ReasoningLevel,
    /// Active tool names at the abandoned leaf.
    pub active_tool_names: Vec<String>,
    /// Maximum approximate tokens selected from the abandoned branch.
    pub token_budget: u64,
    /// Model used for the standalone summary call.
    pub summary_model: ModelRef,
    /// Preallocated durable branch-summary entry.
    pub result_entry_id: EntryId,
    /// Stable host timestamp for the standalone request.
    pub timestamp: Timestamp,
}

/// Generated durable branch summary.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummaryResult {
    /// Model-visible summary including Pi's returned-branch preamble.
    pub summary: String,
    /// Policy-owned persisted details.
    pub details: Option<VersionedExtension>,
    /// Provider usage incurred by summarization.
    pub usage: Option<Usage>,
    /// Provider-priced summary cost, when known.
    pub cost: Option<Cost>,
    /// Terminal reason used by durable usage attribution.
    pub stop_reason: AssistantFinishReason,
}

/// Send-capable branch-summary policy from Architecture v2 part 2 §7.8.
pub trait BranchSummaryPolicy: Send + Sync + 'static {
    /// Summarizes one abandoned branch segment.
    fn summarize(
        &self,
        input: BranchSummaryInput,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>>;
}

/// Local-executor branch-summary policy counterpart from Architecture v2 part 2 §9.2.
pub trait LocalBranchSummaryPolicy: 'static {
    /// Summarizes one abandoned branch segment without requiring `Send`.
    fn summarize(
        &self,
        input: BranchSummaryInput,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>>;
}

/// Model-backed Pi-compatible branch-summary policy.
pub struct RuntimeBranchSummaryPolicy {
    runtime: Arc<dyn ModelRuntime>,
    summary_model: ModelRef,
    context_window: u64,
    reserve_tokens: u64,
    options: SimpleGenerationOptions,
}

impl RuntimeBranchSummaryPolicy {
    /// Creates a policy with Pi's 16,384-token reserve.
    pub fn new(
        runtime: Arc<dyn ModelRuntime>,
        summary_model: ModelRef,
        context_window: u64,
    ) -> Self {
        Self {
            runtime,
            summary_model,
            context_window,
            reserve_tokens: 16_384,
            options: SimpleGenerationOptions::default(),
        }
    }

    /// Replaces the reserve used to bound selected abandoned history.
    pub fn with_reserve_tokens(mut self, reserve_tokens: u64) -> Self {
        self.reserve_tokens = reserve_tokens;
        self
    }

    /// Replaces common request options before summary-specific overrides.
    pub fn with_options(mut self, options: SimpleGenerationOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns the configured summary model.
    pub fn summary_model(&self) -> &ModelRef {
        &self.summary_model
    }

    /// Returns the Pi-compatible abandoned-history token budget.
    pub fn token_budget(&self) -> u64 {
        branch_summary_token_budget(self.context_window, self.reserve_tokens)
    }
}

impl BranchSummaryPolicy for RuntimeBranchSummaryPolicy {
    fn summarize(
        &self,
        mut input: BranchSummaryInput,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>> {
        Box::pin(async move {
            if input.summary_model.model.as_str().is_empty() {
                input.summary_model = self.summary_model.clone();
            }
            if input.token_budget == 0 {
                input.token_budget = self.token_budget();
            }
            let prepared = branch_summary_request(&input, &self.options)?;
            if prepared.request.context.messages.is_empty() {
                return Ok(empty_branch_summary());
            }
            let mut stream = self.runtime.stream(prepared.request, cancellation).await?;
            while let Some(event) = stream.next().await {
                if let Some(message) = event.terminal_message() {
                    return branch_result_from_terminal(message.clone(), prepared.file_operations);
                }
            }
            Err(BranchSummaryError::summarization(
                "branch summary stream ended without a terminal assistant message",
            ))
        })
    }
}

/// Local-runtime model-backed Pi-compatible branch-summary policy.
pub struct LocalRuntimeBranchSummaryPolicy {
    runtime: Rc<dyn LocalModelRuntime>,
    summary_model: ModelRef,
    context_window: u64,
    reserve_tokens: u64,
    options: SimpleGenerationOptions,
}

impl LocalRuntimeBranchSummaryPolicy {
    /// Creates a local policy with Pi's 16,384-token reserve.
    pub fn new(
        runtime: Rc<dyn LocalModelRuntime>,
        summary_model: ModelRef,
        context_window: u64,
    ) -> Self {
        Self {
            runtime,
            summary_model,
            context_window,
            reserve_tokens: 16_384,
            options: SimpleGenerationOptions::default(),
        }
    }

    /// Replaces the reserve used to bound selected abandoned history.
    pub fn with_reserve_tokens(mut self, reserve_tokens: u64) -> Self {
        self.reserve_tokens = reserve_tokens;
        self
    }

    /// Replaces common request options before summary-specific overrides.
    pub fn with_options(mut self, options: SimpleGenerationOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns the Pi-compatible abandoned-history token budget.
    pub fn token_budget(&self) -> u64 {
        branch_summary_token_budget(self.context_window, self.reserve_tokens)
    }
}

impl LocalBranchSummaryPolicy for LocalRuntimeBranchSummaryPolicy {
    fn summarize(
        &self,
        mut input: BranchSummaryInput,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>> {
        Box::pin(async move {
            if input.summary_model.model.as_str().is_empty() {
                input.summary_model = self.summary_model.clone();
            }
            if input.token_budget == 0 {
                input.token_budget = self.token_budget();
            }
            let prepared = branch_summary_request(&input, &self.options)?;
            if prepared.request.context.messages.is_empty() {
                return Ok(empty_branch_summary());
            }
            let mut stream = self.runtime.stream(prepared.request, cancellation).await?;
            while let Some(event) = stream.next().await {
                if let Some(message) = event.terminal_message() {
                    return branch_result_from_terminal(message.clone(), prepared.file_operations);
                }
            }
            Err(BranchSummaryError::summarization(
                "branch summary stream ended without a terminal assistant message",
            ))
        })
    }
}

/// Durable branch-navigation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchNavigationResult {
    /// New lane leaf after navigation.
    pub new_leaf_id: Option<EntryId>,
    /// Durable summary entry appended under the target, when requested.
    pub summary_entry_id: Option<EntryId>,
    /// Common ancestor used to collect the abandoned segment.
    pub common_ancestor_id: Option<EntryId>,
}

/// Orchestrates durable navigation over a selected [`Session`] lane.
pub struct BranchNavigator {
    /// Selected durable session lane.
    pub session: Arc<Session>,
    /// Abandoned-branch summary behavior.
    pub policy: Arc<dyn BranchSummaryPolicy>,
    /// Model used for summary calls.
    pub summary_model: ModelRef,
    /// Maximum abandoned-history estimate selected for a summary call.
    pub token_budget: u64,
}

impl BranchNavigator {
    /// Navigates to `target_id`, optionally committing a summary under the target.
    pub async fn navigate(
        &self,
        target_id: Option<EntryId>,
        summarize: bool,
        custom_instructions: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        cancellation
            .check()
            .map_err(|_| BranchSummaryError::Cancelled)?;
        let state = self.session.load_state().await?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(BranchSummaryError::InvalidTarget {
                target_id: target.clone(),
            });
        }
        let old_leaf = state
            .lane_leaf(self.session.lane())
            .ok_or_else(|| BranchSummaryError::InvalidLane {
                lane: self.session.lane().clone(),
            })?
            .clone();
        let summary_entry_id = summarize.then(|| self.session.next_entry_id("branch-summary"));
        let run_id = self
            .session
            .start_operation(OperationIntent::Navigation {
                target_id: target_id.clone(),
                summarize,
                custom_instructions: custom_instructions.clone(),
                label: None,
                summary_entry_id: summary_entry_id.clone(),
            })
            .await?;
        if !summarize {
            self.session.move_lane(target_id.clone()).await?;
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: target_id,
                summary_entry_id: None,
                common_ancestor_id: None,
            });
        }
        let entry_id = summary_entry_id.expect("summarizing navigation preallocates an entry");
        self.execute_summary_navigation(
            &state,
            run_id,
            old_leaf,
            target_id,
            entry_id,
            custom_instructions,
            true,
            cancellation,
        )
        .await
    }

    /// Resumes the selected lane's incomplete durable summarized navigation.
    pub async fn resume(
        &self,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        let state = self.session.load_state().await?;
        let open = state.open_operations(self.session.lane());
        let operation = match open.as_slice() {
            [operation] => (*operation).clone(),
            [] => {
                return Err(BranchSummaryError::NotResumable {
                    message: "the selected lane has no open navigation".to_owned(),
                });
            }
            _ => {
                return Err(BranchSummaryError::NotResumable {
                    message: "the selected lane has multiple open operations".to_owned(),
                });
            }
        };
        let OperationRecord::Started {
            source_leaf_id,
            intent:
                OperationIntent::Navigation {
                    target_id,
                    summarize,
                    custom_instructions,
                    summary_entry_id,
                    ..
                },
            ..
        } = &operation
        else {
            return Err(BranchSummaryError::NotResumable {
                message: "open operation is not a navigation".to_owned(),
            });
        };
        let run_id =
            started_run_id(&operation).ok_or_else(|| BranchSummaryError::NotResumable {
                message: "open navigation has no run identity".to_owned(),
            })?;
        if !summarize {
            let current_leaf = state.lane_leaf(self.session.lane()).ok_or_else(|| {
                BranchSummaryError::InvalidLane {
                    lane: self.session.lane().clone(),
                }
            })?;
            if current_leaf != target_id {
                self.session.move_lane(target_id.clone()).await?;
            }
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: target_id.clone(),
                summary_entry_id: None,
                common_ancestor_id: None,
            });
        }
        let target_id = target_id.clone();
        let summary_entry_id =
            summary_entry_id
                .clone()
                .ok_or_else(|| BranchSummaryError::NotResumable {
                    message: "summarized navigation has no result entry identity".to_owned(),
                })?;
        if state.entry(&summary_entry_id).is_some() {
            let common_ancestor_id = collect_entries_for_branch_summary(
                &state,
                source_leaf_id.as_ref(),
                target_id.as_ref(),
            )?
            .common_ancestor_id;
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: Some(summary_entry_id.clone()),
                summary_entry_id: Some(summary_entry_id),
                common_ancestor_id,
            });
        }
        self.execute_summary_navigation(
            &state,
            run_id,
            source_leaf_id.clone(),
            target_id,
            summary_entry_id,
            custom_instructions.clone(),
            true,
            cancellation,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "durable navigation execution carries its complete recovery identity"
    )]
    async fn execute_summary_navigation(
        &self,
        state: &SessionState,
        run_id: agentprism_ai::RunId,
        old_leaf: Option<EntryId>,
        target_id: Option<EntryId>,
        summary_entry_id: EntryId,
        custom_instructions: Option<String>,
        owns_operation: bool,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        let collected =
            collect_entries_for_branch_summary(state, old_leaf.as_ref(), target_id.as_ref())?;
        let from_id = old_leaf
            .clone()
            .ok_or_else(|| BranchSummaryError::NotResumable {
                message: "summarized navigation has no source leaf".to_owned(),
            })?;
        let attempt = next_step_attempt(state, &run_id, OperationStep::BranchSummary);
        self.session
            .append_step_attempt(
                run_id.clone(),
                OperationStep::BranchSummary,
                attempt,
                summary_entry_id.clone(),
                None,
            )
            .await?;
        let (active_model, reasoning, active_tool_names) =
            derive_branch_state(state, old_leaf.as_ref())?;
        let result = self
            .policy
            .summarize(
                BranchSummaryInput {
                    common_ancestor_id: collected.common_ancestor_id.clone(),
                    abandoned_entries: collected.entries.clone(),
                    target_tail: collected.target_tail,
                    custom_instructions,
                    replace_instructions: false,
                    active_model,
                    reasoning,
                    active_tool_names,
                    token_budget: self.token_budget,
                    summary_model: self.summary_model.clone(),
                    result_entry_id: summary_entry_id.clone(),
                    timestamp: target_id
                        .as_ref()
                        .and_then(|target| state.entry(target))
                        .map_or(Timestamp::default(), |entry| entry.base().timestamp),
                },
                cancellation,
            )
            .await?;
        self.session
            .commit_branch_summary(
                run_id.clone(),
                attempt,
                target_id,
                summary_entry_id.clone(),
                from_id,
                result.summary,
                result.details,
                result.usage,
                result.cost,
                result.stop_reason,
            )
            .await?;
        if owns_operation {
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
        }
        Ok(BranchNavigationResult {
            new_leaf_id: Some(summary_entry_id.clone()),
            summary_entry_id: Some(summary_entry_id),
            common_ancestor_id: collected.common_ancestor_id,
        })
    }
}

/// Single-threaded durable branch navigator over [`LocalSession`].
pub struct LocalBranchNavigator {
    /// Selected local durable session lane.
    pub session: Rc<LocalSession>,
    /// Local abandoned-branch summary behavior.
    pub policy: Rc<dyn LocalBranchSummaryPolicy>,
    /// Model used for summary calls.
    pub summary_model: ModelRef,
    /// Maximum abandoned-history estimate selected for a summary call.
    pub token_budget: u64,
}

impl LocalBranchNavigator {
    /// Navigates to an entry or the empty root, optionally committing a summary.
    pub async fn navigate(
        &self,
        target_id: Option<EntryId>,
        summarize: bool,
        custom_instructions: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        cancellation
            .check()
            .map_err(|_| BranchSummaryError::Cancelled)?;
        let state = self.session.load_state().await?;
        if let Some(target) = &target_id
            && state.entry(target).is_none()
        {
            return Err(BranchSummaryError::InvalidTarget {
                target_id: target.clone(),
            });
        }
        let old_leaf = state
            .lane_leaf(self.session.lane())
            .ok_or_else(|| BranchSummaryError::InvalidLane {
                lane: self.session.lane().clone(),
            })?
            .clone();
        let summary_entry_id = summarize.then(|| self.session.next_entry_id("branch-summary"));
        let run_id = self
            .session
            .start_operation(OperationIntent::Navigation {
                target_id: target_id.clone(),
                summarize,
                custom_instructions: custom_instructions.clone(),
                label: None,
                summary_entry_id: summary_entry_id.clone(),
            })
            .await?;
        if !summarize {
            self.session.move_lane(target_id.clone()).await?;
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: target_id,
                summary_entry_id: None,
                common_ancestor_id: None,
            });
        }
        let entry_id = summary_entry_id.expect("summarizing navigation preallocates an entry");
        self.execute_summary_navigation(
            &state,
            run_id,
            old_leaf,
            target_id,
            entry_id,
            custom_instructions,
            true,
            cancellation,
        )
        .await
    }

    /// Resumes the selected lane's incomplete local navigation.
    pub async fn resume(
        &self,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        let state = self.session.load_state().await?;
        let open = state.open_operations(self.session.lane());
        let operation = match open.as_slice() {
            [operation] => (*operation).clone(),
            [] => {
                return Err(BranchSummaryError::NotResumable {
                    message: "the selected lane has no open navigation".to_owned(),
                });
            }
            _ => {
                return Err(BranchSummaryError::NotResumable {
                    message: "the selected lane has multiple open operations".to_owned(),
                });
            }
        };
        let OperationRecord::Started {
            source_leaf_id,
            intent:
                OperationIntent::Navigation {
                    target_id,
                    summarize,
                    custom_instructions,
                    summary_entry_id,
                    ..
                },
            ..
        } = &operation
        else {
            return Err(BranchSummaryError::NotResumable {
                message: "open operation is not a navigation".to_owned(),
            });
        };
        let run_id =
            started_run_id(&operation).ok_or_else(|| BranchSummaryError::NotResumable {
                message: "open navigation has no run identity".to_owned(),
            })?;
        if !summarize {
            let current_leaf = state.lane_leaf(self.session.lane()).ok_or_else(|| {
                BranchSummaryError::InvalidLane {
                    lane: self.session.lane().clone(),
                }
            })?;
            if current_leaf != target_id {
                self.session.move_lane(target_id.clone()).await?;
            }
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: target_id.clone(),
                summary_entry_id: None,
                common_ancestor_id: None,
            });
        }
        let summary_entry_id =
            summary_entry_id
                .clone()
                .ok_or_else(|| BranchSummaryError::NotResumable {
                    message: "summarized navigation has no result entry identity".to_owned(),
                })?;
        if state.entry(&summary_entry_id).is_some() {
            let common_ancestor_id = collect_entries_for_branch_summary(
                &state,
                source_leaf_id.as_ref(),
                target_id.as_ref(),
            )?
            .common_ancestor_id;
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
            return Ok(BranchNavigationResult {
                new_leaf_id: Some(summary_entry_id.clone()),
                summary_entry_id: Some(summary_entry_id),
                common_ancestor_id,
            });
        }
        self.execute_summary_navigation(
            &state,
            run_id,
            source_leaf_id.clone(),
            target_id.clone(),
            summary_entry_id,
            custom_instructions.clone(),
            true,
            cancellation,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "durable navigation execution carries its complete recovery identity"
    )]
    async fn execute_summary_navigation(
        &self,
        state: &SessionState,
        run_id: agentprism_ai::RunId,
        old_leaf: Option<EntryId>,
        target_id: Option<EntryId>,
        summary_entry_id: EntryId,
        custom_instructions: Option<String>,
        owns_operation: bool,
        cancellation: CancellationToken,
    ) -> Result<BranchNavigationResult, BranchSummaryError> {
        let collected =
            collect_entries_for_branch_summary(state, old_leaf.as_ref(), target_id.as_ref())?;
        let from_id = old_leaf.ok_or_else(|| BranchSummaryError::NotResumable {
            message: "summarized navigation has no source leaf".to_owned(),
        })?;
        let attempt = next_step_attempt(state, &run_id, OperationStep::BranchSummary);
        self.session
            .append_step_attempt(
                run_id.clone(),
                OperationStep::BranchSummary,
                attempt,
                summary_entry_id.clone(),
                None,
            )
            .await?;
        let (active_model, reasoning, active_tool_names) =
            derive_branch_state(state, Some(&from_id))?;
        let result = self
            .policy
            .summarize(
                BranchSummaryInput {
                    common_ancestor_id: collected.common_ancestor_id.clone(),
                    abandoned_entries: collected.entries.clone(),
                    target_tail: collected.target_tail,
                    custom_instructions,
                    replace_instructions: false,
                    active_model,
                    reasoning,
                    active_tool_names,
                    token_budget: self.token_budget,
                    summary_model: self.summary_model.clone(),
                    result_entry_id: summary_entry_id.clone(),
                    timestamp: target_id
                        .as_ref()
                        .and_then(|target| state.entry(target))
                        .map_or(Timestamp::default(), |entry| entry.base().timestamp),
                },
                cancellation,
            )
            .await?;
        self.session
            .commit_branch_summary(
                run_id.clone(),
                attempt,
                target_id,
                summary_entry_id.clone(),
                from_id,
                result.summary,
                result.details,
                result.usage,
                result.cost,
                result.stop_reason,
            )
            .await?;
        if owns_operation {
            self.session
                .finish_operation(run_id, OperationOutcome::Completed, None)
                .await?;
        }
        Ok(BranchNavigationResult {
            new_leaf_id: Some(summary_entry_id.clone()),
            summary_entry_id: Some(summary_entry_id),
            common_ancestor_id: collected.common_ancestor_id,
        })
    }
}

/// Collects the abandoned side and target tail around their deepest common ancestor.
pub fn collect_entries_for_branch_summary(
    state: &SessionState,
    old_leaf_id: Option<&EntryId>,
    target_id: Option<&EntryId>,
) -> Result<CollectedBranchEntries, BranchSummaryError> {
    let target_path = if let Some(target_id) = target_id {
        state.scan_branch_root_to_leaf(target_id).map_err(|_| {
            BranchSummaryError::InvalidTarget {
                target_id: target_id.clone(),
            }
        })?
    } else {
        Vec::new()
    };
    let Some(old_leaf_id) = old_leaf_id else {
        return Ok(CollectedBranchEntries {
            entries: Vec::new(),
            common_ancestor_id: None,
            target_tail: target_path.into_iter().cloned().collect(),
        });
    };
    let old_path = state.scan_branch_root_to_leaf(old_leaf_id).map_err(|_| {
        BranchSummaryError::InvalidTarget {
            target_id: old_leaf_id.clone(),
        }
    })?;
    let old_ids = old_path
        .iter()
        .map(|entry| entry.id().clone())
        .collect::<BTreeSet<_>>();
    let common_ancestor_id = target_path
        .iter()
        .rev()
        .find(|entry| old_ids.contains(entry.id()))
        .map(|entry| entry.id().clone());
    let abandoned_start = common_ancestor_id
        .as_ref()
        .and_then(|id| old_path.iter().position(|entry| entry.id() == id))
        .map_or(0, |index| index.saturating_add(1));
    let target_start = common_ancestor_id
        .as_ref()
        .and_then(|id| target_path.iter().position(|entry| entry.id() == id))
        .map_or(0, |index| index.saturating_add(1));
    Ok(CollectedBranchEntries {
        entries: old_path[abandoned_start..]
            .iter()
            .map(|entry| (*entry).clone())
            .collect(),
        common_ancestor_id,
        target_tail: target_path[target_start..]
            .iter()
            .map(|entry| (*entry).clone())
            .collect(),
    })
}

fn branch_summary_request(
    input: &BranchSummaryInput,
    base_options: &SimpleGenerationOptions,
) -> Result<PreparedBranchRequest, BranchSummaryError> {
    let (records, file_operations) =
        prepare_branch_records(&input.abandoned_entries, input.token_budget)?;
    if records.is_empty() {
        return Ok(PreparedBranchRequest {
            request: ModelRequest {
                model: input.summary_model.clone(),
                context: Context::new(Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned())),
                options: base_options.clone(),
            },
            file_operations,
        });
    }
    let conversation = serialize_conversation(&records)
        .map_err(|error| BranchSummaryError::summarization(error.to_string()))?;
    let custom_instructions = input
        .custom_instructions
        .as_deref()
        .filter(|instructions| !instructions.is_empty());
    let instructions = match (custom_instructions, input.replace_instructions) {
        (Some(custom), true) => custom.to_owned(),
        (Some(custom), false) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        (None, _) => BRANCH_SUMMARY_PROMPT.to_owned(),
    };
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}");
    let mut context = Context::new(Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new(format!("{}-branch-user", input.result_entry_id)),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{}-branch-text", input.result_entry_id)),
            text: prompt,
        }],
        timestamp: input.timestamp,
    }));
    let mut options = base_options.clone();
    options.max_output_tokens = Some(2_048);
    options.cache_retention = Some(CacheRetention::None);
    options.session_id = Some(format!("{}-branch-session", input.result_entry_id));
    Ok(PreparedBranchRequest {
        request: ModelRequest {
            model: input.summary_model.clone(),
            context,
            options,
        },
        file_operations,
    })
}

struct PreparedBranchRequest {
    request: ModelRequest,
    file_operations: FileOperations,
}

fn prepare_branch_records(
    entries: &[SessionEntry],
    token_budget: u64,
) -> Result<(Vec<AgentRecord>, FileOperations), BranchSummaryError> {
    let mut file_operations = FileOperations::default();
    for entry in entries {
        if let SessionEntry::BranchSummary { details, .. } = entry {
            file_operations.extend_details(details.as_ref());
        }
    }
    let mut selected = Vec::new();
    let mut total = 0_u64;
    for entry in entries.iter().rev() {
        let Some((record, structural_summary)) = entry_to_branch_record(entry) else {
            continue;
        };
        file_operations.extract_record(&record);
        let tokens = estimate_record_tokens(&record)
            .map_err(|error| BranchSummaryError::summarization(error.to_string()))?;
        if token_budget > 0 && total.saturating_add(tokens) > token_budget {
            if structural_summary
                && u128::from(total).saturating_mul(10) < u128::from(token_budget).saturating_mul(9)
            {
                selected.push(record);
            }
            break;
        }
        total = total.saturating_add(tokens);
        selected.push(record);
    }
    selected.reverse();
    Ok((selected, file_operations))
}

fn entry_to_branch_record(entry: &SessionEntry) -> Option<(AgentRecord, bool)> {
    match entry {
        SessionEntry::Message {
            message: AgentRecord::Llm(Message::ToolResult(_)),
            ..
        } => None,
        SessionEntry::Message { message, .. } => Some((message.clone(), false)),
        SessionEntry::Compaction {
            base,
            summary,
            tokens_before,
            ..
        } => Some((
            compaction_summary_record(summary, *tokens_before, base.timestamp),
            true,
        )),
        SessionEntry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } => Some((
            branch_summary_record(summary, from_id, base.timestamp),
            true,
        )),
        SessionEntry::ModelChange { .. }
        | SessionEntry::ReasoningChange { .. }
        | SessionEntry::ActiveToolsChange { .. }
        | SessionEntry::Custom { .. } => None,
    }
}

fn branch_result_from_terminal(
    message: AssistantMessage,
    file_operations: FileOperations,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    match message.finish.reason {
        AssistantFinishReason::Aborted => Err(BranchSummaryError::Cancelled),
        AssistantFinishReason::Error => Err(BranchSummaryError::summarization(format!(
            "Branch summary failed: {}",
            message
                .finish
                .error
                .as_ref()
                .map_or("Unknown error", |error| error.message.as_str())
        ))),
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => {
            let (read_files, modified_files) = file_operations.lists();
            let summary = format!(
                "{BRANCH_SUMMARY_PREAMBLE}{}{}",
                assistant_text(&message),
                format_file_operations(&read_files, &modified_files)
            );
            Ok(BranchSummaryResult {
                summary: if summary.is_empty() {
                    "No summary generated".to_owned()
                } else {
                    summary
                },
                details: Some(file_operation_details(read_files, modified_files)),
                usage: Some(message.usage),
                cost: message.cost,
                stop_reason: message.finish.reason,
            })
        }
    }
}

fn empty_branch_summary() -> BranchSummaryResult {
    BranchSummaryResult {
        summary: "No content to summarize".to_owned(),
        details: Some(file_operation_details(Vec::new(), Vec::new())),
        usage: None,
        cost: None,
        stop_reason: AssistantFinishReason::Stop,
    }
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn derive_branch_state(
    state: &SessionState,
    leaf: Option<&EntryId>,
) -> Result<(Option<ModelRef>, ReasoningLevel, Vec<String>), BranchSummaryError> {
    let Some(leaf) = leaf else {
        return Ok((None, ReasoningLevel::Off, Vec::new()));
    };
    let path =
        state
            .scan_branch_root_to_leaf(leaf)
            .map_err(|_| BranchSummaryError::InvalidTarget {
                target_id: leaf.clone(),
            })?;
    let mut model = None;
    let mut reasoning = ReasoningLevel::Off;
    let mut tools = Vec::new();
    for entry in path {
        match entry {
            SessionEntry::ModelChange { model: changed, .. } => model = Some(changed.clone()),
            SessionEntry::ReasoningChange { level, .. } => reasoning = *level,
            SessionEntry::ActiveToolsChange { tool_names, .. } => tools = tool_names.clone(),
            SessionEntry::Message {
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } => {
                model = Some(ModelRef {
                    provider: assistant.provider.clone(),
                    model: assistant.requested_model.clone(),
                });
            }
            _ => {}
        }
    }
    Ok((model, reasoning, tools))
}

fn branch_summary_token_budget(context_window: u64, reserve_tokens: u64) -> u64 {
    let context_window = if context_window == 0 {
        DEFAULT_CONTEXT_WINDOW
    } else {
        context_window
    };
    context_window.saturating_sub(reserve_tokens)
}
