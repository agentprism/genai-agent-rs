//! Compaction decisions and model-backed summary generation.

use crate::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX, CompactionError,
    file_operations::{FileOperations, file_operation_details, format_file_operations},
};
use futures_util::StreamExt;
use pi_agent_core::AgentRecord;
use pi_agent_session::{CompactionReason, EntryId, SessionEntry};
use pi_ai::{
    AssistantFinishReason, AssistantMessage, CacheRetention, CancellationToken, ContentBlock,
    ContentBlockId, Context, ContextUsageEstimate, LocalBoxFuture, LocalModelRuntime, Message,
    MessageId, ModelRef, ModelRequest, ModelRuntime, ReasoningLevel, SendBoxFuture,
    SimpleGenerationOptions, Timestamp, ToolResultContent, Usage, UsageSource, UserMessage,
    VersionedExtension,
};
use serde_json::{Map, Value, value::RawValue};
use std::{fmt::Write as _, rc::Rc, sync::Arc};

/// System instruction used by pinned Pi for standalone summary requests.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

pub(crate) const BASH_EXECUTION_ROLE: &str = "bashExecution";
pub(crate) const CUSTOM_ROLE: &str = "custom";
pub(crate) const BRANCH_SUMMARY_ROLE: &str = "branchSummary";
pub(crate) const COMPACTION_SUMMARY_ROLE: &str = "compactionSummary";

/// Threshold and tail-retention settings from pinned Pi compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionSettings {
    /// Enables threshold-triggered compaction. Manual and overflow triggers remain available.
    pub enabled: bool,
    /// Tokens reserved for summary prompt and output.
    pub reserve_tokens: u64,
    /// Approximate recent-context tokens retained after compaction.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// Borrowed inputs used for one deterministic compaction decision.
pub struct CompactionDecisionInput<'a> {
    /// Chronological records reconstructed from the selected session branch.
    pub records: &'a [AgentRecord],
    /// Record indexes originating from structural branch-summary entries.
    ///
    /// Pinned Pi treats those entries as valid cut points but does not count
    /// their summary text while accumulating the retained-tail token budget.
    /// A branch-summary message already stored inside a message entry is not
    /// structural and therefore must not appear here.
    pub structural_branch_summary_indices: &'a [usize],
    /// Pi-equivalent estimated provider context tokens.
    pub context_tokens: u64,
    /// Catalog context window for the current request model.
    pub context_window: u64,
    /// Explicit trigger that bypasses threshold enablement.
    pub requested_reason: Option<CompactionReason>,
    /// Model selected for the pending assistant request.
    pub current_model: &'a ModelRef,
}

/// Result of deterministic compaction planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionDecision {
    /// Preserve the reconstructed context unchanged.
    NoCompaction,
    /// Generate a summary and retain the configured tail.
    Compact {
        /// Trigger recorded on the durable step attempt.
        reason: CompactionReason,
        /// First record retained directly on the compaction entry.
        retained_tail_start: usize,
        /// Model used for the standalone summary call.
        summary_model: ModelRef,
    },
}

/// Owned input for a compaction summary call.
#[derive(Clone, Debug)]
pub struct CompactionInput {
    /// Chronological branch records visible before this compaction.
    pub records: Vec<AgentRecord>,
    /// Record indexes originating from structural branch-summary entries.
    ///
    /// This identity must survive cut selection because pinned Pi treats a
    /// structural `branch_summary` entry as the start of a turn. A
    /// branch-summary record stored inside a message entry is not structural
    /// and therefore must not appear here.
    pub structural_branch_summary_indices: Vec<usize>,
    /// First record retained directly after the summary.
    pub retained_tail_start: usize,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Trigger recorded in the operation log.
    pub reason: CompactionReason,
    /// Model used for summarization.
    pub summary_model: ModelRef,
    /// Preallocated durable result entry.
    pub result_entry_id: EntryId,
    /// Previous iterative summary, if the branch already contains a compaction.
    pub previous_summary: Option<String>,
    /// Previous compaction's policy details for cumulative file-operation metadata.
    pub previous_details: Option<VersionedExtension>,
    /// Optional caller focus appended to the summary instructions.
    pub custom_instructions: Option<String>,
    /// Reasoning level used only when the summary model supports it.
    pub reasoning: ReasoningLevel,
    /// Stable host timestamp for the standalone request.
    pub timestamp: Timestamp,
}

/// Generated compaction data ready for one durable compaction entry.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionResult {
    /// Summary replacing the compacted prefix.
    pub summary: String,
    /// Recent records retained verbatim.
    pub retained_tail: Vec<AgentRecord>,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Policy-owned persisted details.
    pub details: Option<VersionedExtension>,
    /// Combined provider usage from summary calls.
    pub usage: Option<Usage>,
    /// Provider-priced summary cost when a single compatible cost is available.
    pub cost: Option<pi_ai::Cost>,
    /// Terminal reason used by durable usage attribution.
    pub stop_reason: AssistantFinishReason,
}

/// Send-capable compaction policy from Architecture v2 part 2 §7.7.
pub trait CompactionPolicy: Send + Sync + 'static {
    /// Makes a synchronous deterministic threshold/manual/overflow decision.
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError>;

    /// Generates one replay-safe summary result.
    fn compact(
        &self,
        input: CompactionInput,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CompactionResult, CompactionError>>;
}

/// Local-executor compaction policy counterpart from Architecture v2 part 2 §9.2.
pub trait LocalCompactionPolicy: 'static {
    /// Makes a synchronous deterministic threshold/manual/overflow decision.
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError>;

    /// Generates one local replay-safe summary result.
    fn compact(
        &self,
        input: CompactionInput,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CompactionResult, CompactionError>>;
}

/// Model-backed Pi-compatible threshold compaction policy.
pub struct RuntimeCompactionPolicy {
    runtime: Arc<dyn ModelRuntime>,
    summary_model: ModelRef,
    context_window: u64,
    summary_model_max_output: u32,
    summary_model_reasoning: bool,
    settings: CompactionSettings,
    options: SimpleGenerationOptions,
}

impl RuntimeCompactionPolicy {
    /// Creates a model-backed policy with Pi's default summary settings.
    pub fn new(
        runtime: Arc<dyn ModelRuntime>,
        summary_model: ModelRef,
        context_window: u64,
        summary_model_max_output: u32,
        summary_model_reasoning: bool,
        settings: CompactionSettings,
    ) -> Self {
        Self {
            runtime,
            summary_model,
            context_window,
            summary_model_max_output,
            summary_model_reasoning,
            settings,
            options: SimpleGenerationOptions::default(),
        }
    }

    /// Replaces common summary request options before Pi's cache/session/max overrides.
    pub fn with_options(mut self, options: SimpleGenerationOptions) -> Self {
        self.options = options;
        self
    }
}

impl CompactionPolicy for RuntimeCompactionPolicy {
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError> {
        decide_with_settings(
            input,
            self.settings,
            &self.summary_model,
            self.context_window,
        )
    }

    fn compact(
        &self,
        input: CompactionInput,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CompactionResult, CompactionError>> {
        Box::pin(async move {
            compact_with_send_runtime(
                self.runtime.as_ref(),
                input,
                self.settings,
                self.summary_model_max_output,
                self.summary_model_reasoning,
                &self.options,
                cancellation,
            )
            .await
        })
    }
}

/// Local-runtime model-backed Pi-compatible threshold compaction policy.
pub struct LocalRuntimeCompactionPolicy {
    runtime: Rc<dyn LocalModelRuntime>,
    summary_model: ModelRef,
    context_window: u64,
    summary_model_max_output: u32,
    summary_model_reasoning: bool,
    settings: CompactionSettings,
    options: SimpleGenerationOptions,
}

impl LocalRuntimeCompactionPolicy {
    /// Creates a local model-backed policy with Pi's default summary settings.
    pub fn new(
        runtime: Rc<dyn LocalModelRuntime>,
        summary_model: ModelRef,
        context_window: u64,
        summary_model_max_output: u32,
        summary_model_reasoning: bool,
        settings: CompactionSettings,
    ) -> Self {
        Self {
            runtime,
            summary_model,
            context_window,
            summary_model_max_output,
            summary_model_reasoning,
            settings,
            options: SimpleGenerationOptions::default(),
        }
    }

    /// Replaces common summary request options before Pi's cache/session/max overrides.
    pub fn with_options(mut self, options: SimpleGenerationOptions) -> Self {
        self.options = options;
        self
    }
}

impl LocalCompactionPolicy for LocalRuntimeCompactionPolicy {
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError> {
        decide_with_settings(
            input,
            self.settings,
            &self.summary_model,
            self.context_window,
        )
    }

    fn compact(
        &self,
        input: CompactionInput,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CompactionResult, CompactionError>> {
        Box::pin(async move {
            compact_with_local_runtime(
                self.runtime.as_ref(),
                input,
                self.settings,
                self.summary_model_max_output,
                self.summary_model_reasoning,
                &self.options,
                cancellation,
            )
            .await
        })
    }
}

fn decide_with_settings(
    input: CompactionDecisionInput<'_>,
    settings: CompactionSettings,
    summary_model: &ModelRef,
    configured_context_window: u64,
) -> Result<CompactionDecision, CompactionError> {
    let reason = if let Some(reason) = input.requested_reason {
        reason
    } else {
        let context_window = if input.context_window == 0 {
            configured_context_window
        } else {
            input.context_window
        };
        if !should_compact(input.context_tokens, context_window, settings) {
            return Ok(CompactionDecision::NoCompaction);
        }
        CompactionReason::Threshold
    };
    Ok(CompactionDecision::Compact {
        reason,
        retained_tail_start: find_retained_tail_start_with_structural_summaries(
            input.records,
            settings.keep_recent_tokens,
            input.structural_branch_summary_indices,
        )?,
        summary_model: summary_model.clone(),
    })
}

/// Applies pinned Pi's strict `context > window - reserve` threshold.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: CompactionSettings,
) -> bool {
    settings.enabled
        && i128::from(context_tokens)
            > i128::from(context_window).saturating_sub(i128::from(settings.reserve_tokens))
}

/// Estimates one agent record with Pi's four-UTF-16-code-units-per-token rule.
pub fn estimate_record_tokens(record: &AgentRecord) -> Result<u64, CompactionError> {
    match record {
        AgentRecord::Custom { type_name, payload } if is_bash_execution(type_name) => {
            let value = serde_json::from_str::<Value>(payload.get()).ok();
            let object = value.as_ref().and_then(Value::as_object);
            let units = string_field(object, &["command"])
                .encode_utf16()
                .count()
                .saturating_add(string_field(object, &["output"]).encode_utf16().count());
            Ok(utf16_units_to_tokens(units))
        }
        AgentRecord::Custom { type_name, payload }
            if matches!(
                type_name.as_str(),
                BRANCH_SUMMARY_ROLE | COMPACTION_SUMMARY_ROLE
            ) =>
        {
            let value = serde_json::from_str::<Value>(payload.get()).ok();
            let object = value.as_ref().and_then(Value::as_object);
            Ok(utf16_units_to_tokens(
                string_field(object, &["summary"]).encode_utf16().count(),
            ))
        }
        AgentRecord::Custom { type_name, .. } if type_name != CUSTOM_ROLE => Ok(0),
        AgentRecord::Llm(_) | AgentRecord::Custom { .. } => project_record_to_message(record, 0)
            .map_or(Ok(0), |message| {
                pi_ai::estimate_message_tokens(&message).map_err(|error| {
                    CompactionError::decision(format!(
                        "could not estimate compaction record: {error}"
                    ))
                })
            }),
    }
}

fn utf16_units_to_tokens(units: usize) -> u64 {
    u64::try_from(units).unwrap_or(u64::MAX).saturating_add(3) / 4
}

/// Estimates reconstructed harness messages with pinned Pi compaction rules.
///
/// Unlike `pi_ai` request planning, harness compaction considers messages only:
/// it excludes system-prompt and tool-schema overhead, uses the latest valid
/// assistant usage regardless of timestamps, and estimates only messages after
/// that usage block.
pub fn estimate_harness_context_tokens(
    records: &[AgentRecord],
) -> Result<ContextUsageEstimate, CompactionError> {
    let usage_info = records
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, record)| match record {
            AgentRecord::Llm(Message::Assistant(assistant))
                if !matches!(
                    assistant.finish.reason,
                    AssistantFinishReason::Aborted | AssistantFinishReason::Error
                ) && assistant.usage.total_tokens() > 0 =>
            {
                Some((
                    index,
                    u64::try_from(assistant.usage.total_tokens()).unwrap_or(u64::MAX),
                ))
            }
            AgentRecord::Llm(Message::User(_) | Message::Assistant(_) | Message::ToolResult(_))
            | AgentRecord::Custom { .. } => None,
        });
    if let Some((last_usage_index, usage_tokens)) = usage_info {
        let trailing_tokens = records[last_usage_index.saturating_add(1)..]
            .iter()
            .try_fold(0_u64, |total, record| {
                estimate_record_tokens(record).map(|tokens| total.saturating_add(tokens))
            })?;
        return Ok(ContextUsageEstimate {
            tokens: usage_tokens.saturating_add(trailing_tokens),
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(last_usage_index),
        });
    }

    let trailing_tokens = records.iter().try_fold(0_u64, |total, record| {
        estimate_record_tokens(record).map(|tokens| total.saturating_add(tokens))
    })?;
    Ok(ContextUsageEstimate {
        tokens: trailing_tokens,
        usage_tokens: 0,
        trailing_tokens,
        last_usage_index: None,
    })
}

/// Finds the first record retained under the configured approximate token budget.
pub fn find_retained_tail_start(
    records: &[AgentRecord],
    keep_recent_tokens: u64,
) -> Result<usize, CompactionError> {
    find_retained_tail_start_with_structural_summaries(records, keep_recent_tokens, &[])
}

fn find_retained_tail_start_with_structural_summaries(
    records: &[AgentRecord],
    keep_recent_tokens: u64,
    structural_branch_summary_indices: &[usize],
) -> Result<usize, CompactionError> {
    let valid = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| is_valid_cut_record(record).then_some(index))
        .collect::<Vec<_>>();
    let Some(first_valid) = valid.first().copied() else {
        return Ok(0);
    };
    let mut accumulated = 0_u64;
    let mut cut = first_valid;
    for index in (0..records.len()).rev() {
        if !structural_branch_summary_indices.contains(&index) {
            accumulated = accumulated.saturating_add(estimate_record_tokens(&records[index])?);
        }
        if accumulated >= keep_recent_tokens {
            cut = valid
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .unwrap_or(first_valid);
            break;
        }
    }
    // A structural branch summary starts a turn in pinned Pi. When the token
    // threshold lands on the following record, retain that boundary instead
    // of summarizing it again as a nonempty turn prefix.
    while cut > 0 && structural_branch_summary_indices.contains(&(cut - 1)) {
        cut -= 1;
    }
    Ok(cut)
}

fn is_valid_cut_record(record: &AgentRecord) -> bool {
    match record {
        AgentRecord::Llm(Message::ToolResult(_)) => false,
        AgentRecord::Llm(Message::User(_) | Message::Assistant(_)) => true,
        AgentRecord::Custom { type_name, .. } => is_projectable_harness_role(type_name),
    }
}

fn find_turn_start(
    records: &[AgentRecord],
    structural_branch_summary_indices: &[usize],
    index: usize,
) -> Option<usize> {
    (0..=index).rev().find(|candidate| {
        structural_branch_summary_indices.contains(candidate)
            || match &records[*candidate] {
                AgentRecord::Llm(Message::User(_)) => true,
                AgentRecord::Custom { type_name, .. } => is_bash_execution(type_name),
                AgentRecord::Llm(Message::Assistant(_) | Message::ToolResult(_)) => false,
            }
    })
}

#[derive(Debug)]
struct PreparedSummaryRecords {
    history: Vec<AgentRecord>,
    turn_prefix: Vec<AgentRecord>,
    retained_tail: Vec<AgentRecord>,
    split_turn: bool,
    file_operations: FileOperations,
}

fn prepare_summary_records(input: &CompactionInput) -> PreparedSummaryRecords {
    let retained_tail_start = input.retained_tail_start.min(input.records.len());
    let cut_is_user = input
        .records
        .get(retained_tail_start)
        .is_some_and(|record| matches!(record, AgentRecord::Llm(Message::User(_))));
    let turn_start = (!cut_is_user && retained_tail_start < input.records.len())
        .then(|| {
            find_turn_start(
                &input.records,
                &input.structural_branch_summary_indices,
                retained_tail_start,
            )
        })
        .flatten();
    let history_end = turn_start.unwrap_or(retained_tail_start);
    let history = input.records[..history_end].to_vec();
    let turn_prefix = turn_start
        .map(|start| input.records[start..retained_tail_start].to_vec())
        .unwrap_or_default();
    let mut file_operations = FileOperations::default();
    file_operations.extend_details(input.previous_details.as_ref());
    for record in history.iter().chain(&turn_prefix) {
        file_operations.extract_record(record);
    }
    PreparedSummaryRecords {
        history,
        turn_prefix,
        retained_tail: input.records[retained_tail_start..].to_vec(),
        split_turn: turn_start.is_some(),
        file_operations,
    }
}

async fn compact_with_send_runtime(
    runtime: &dyn ModelRuntime,
    input: CompactionInput,
    settings: CompactionSettings,
    model_max_output: u32,
    model_reasoning: bool,
    base_options: &SimpleGenerationOptions,
    cancellation: CancellationToken,
) -> Result<CompactionResult, CompactionError> {
    cancellation
        .check()
        .map_err(|_| CompactionError::Cancelled)?;
    let prepared = prepare_summary_records(&input);
    let (summary, terminal) = if prepared.split_turn && !prepared.turn_prefix.is_empty() {
        let history = if prepared.history.is_empty() {
            None
        } else {
            Some(
                complete_summary_send(
                    runtime,
                    &input,
                    &prepared.history,
                    settings.reserve_tokens,
                    model_max_output,
                    model_reasoning,
                    base_options,
                    false,
                    cancellation.clone(),
                )
                .await?,
            )
        };
        let prefix = complete_summary_send(
            runtime,
            &input,
            &prepared.turn_prefix,
            settings.reserve_tokens,
            model_max_output,
            model_reasoning,
            base_options,
            true,
            cancellation,
        )
        .await?;
        let history_text = history
            .as_ref()
            .map_or_else(|| "No prior history.".to_owned(), assistant_text);
        (
            format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                assistant_text(&prefix)
            ),
            combine_terminal_messages(history.as_ref(), &prefix),
        )
    } else {
        let message = complete_summary_send(
            runtime,
            &input,
            &prepared.history,
            settings.reserve_tokens,
            model_max_output,
            model_reasoning,
            base_options,
            false,
            cancellation,
        )
        .await?;
        (assistant_text(&message), message)
    };
    let (read_files, modified_files) = prepared.file_operations.lists();
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );
    Ok(CompactionResult {
        summary,
        retained_tail: prepared.retained_tail,
        tokens_before: input.tokens_before,
        details: Some(file_operation_details(read_files, modified_files)),
        usage: Some(terminal.usage),
        cost: terminal.cost,
        stop_reason: terminal.finish.reason,
    })
}

async fn compact_with_local_runtime(
    runtime: &dyn LocalModelRuntime,
    input: CompactionInput,
    settings: CompactionSettings,
    model_max_output: u32,
    model_reasoning: bool,
    base_options: &SimpleGenerationOptions,
    cancellation: CancellationToken,
) -> Result<CompactionResult, CompactionError> {
    cancellation
        .check()
        .map_err(|_| CompactionError::Cancelled)?;
    let prepared = prepare_summary_records(&input);
    let (summary, terminal) = if prepared.split_turn && !prepared.turn_prefix.is_empty() {
        let history = if prepared.history.is_empty() {
            None
        } else {
            Some(
                complete_summary_local(
                    runtime,
                    &input,
                    &prepared.history,
                    settings.reserve_tokens,
                    model_max_output,
                    model_reasoning,
                    base_options,
                    false,
                    cancellation.clone(),
                )
                .await?,
            )
        };
        let prefix = complete_summary_local(
            runtime,
            &input,
            &prepared.turn_prefix,
            settings.reserve_tokens,
            model_max_output,
            model_reasoning,
            base_options,
            true,
            cancellation,
        )
        .await?;
        let history_text = history
            .as_ref()
            .map_or_else(|| "No prior history.".to_owned(), assistant_text);
        (
            format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                assistant_text(&prefix)
            ),
            combine_terminal_messages(history.as_ref(), &prefix),
        )
    } else {
        let message = complete_summary_local(
            runtime,
            &input,
            &prepared.history,
            settings.reserve_tokens,
            model_max_output,
            model_reasoning,
            base_options,
            false,
            cancellation,
        )
        .await?;
        (assistant_text(&message), message)
    };
    let (read_files, modified_files) = prepared.file_operations.lists();
    let summary = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );
    Ok(CompactionResult {
        summary,
        retained_tail: prepared.retained_tail,
        tokens_before: input.tokens_before,
        details: Some(file_operation_details(read_files, modified_files)),
        usage: Some(terminal.usage),
        cost: terminal.cost,
        stop_reason: terminal.finish.reason,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "summary request inputs mirror pinned Pi's helper"
)]
async fn complete_summary_send(
    runtime: &dyn ModelRuntime,
    input: &CompactionInput,
    records: &[AgentRecord],
    reserve_tokens: u64,
    model_max_output: u32,
    model_reasoning: bool,
    base_options: &SimpleGenerationOptions,
    turn_prefix: bool,
    cancellation: CancellationToken,
) -> Result<AssistantMessage, CompactionError> {
    let request = summary_request(
        input,
        records,
        reserve_tokens,
        model_max_output,
        model_reasoning,
        base_options,
        turn_prefix,
    )?;
    let mut stream = runtime.stream(request, cancellation).await?;
    while let Some(event) = stream.next().await {
        if let Some(message) = event.terminal_message() {
            return validate_summary_terminal(message.clone(), turn_prefix);
        }
    }
    Err(CompactionError::summarization(
        "summarization stream ended without a terminal assistant message",
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "summary request inputs mirror pinned Pi's helper"
)]
async fn complete_summary_local(
    runtime: &dyn LocalModelRuntime,
    input: &CompactionInput,
    records: &[AgentRecord],
    reserve_tokens: u64,
    model_max_output: u32,
    model_reasoning: bool,
    base_options: &SimpleGenerationOptions,
    turn_prefix: bool,
    cancellation: CancellationToken,
) -> Result<AssistantMessage, CompactionError> {
    let request = summary_request(
        input,
        records,
        reserve_tokens,
        model_max_output,
        model_reasoning,
        base_options,
        turn_prefix,
    )?;
    let mut stream = runtime.stream(request, cancellation).await?;
    while let Some(event) = stream.next().await {
        if let Some(message) = event.terminal_message() {
            return validate_summary_terminal(message.clone(), turn_prefix);
        }
    }
    Err(CompactionError::summarization(
        "summarization stream ended without a terminal assistant message",
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "summary request inputs mirror pinned Pi's helper"
)]
fn summary_request(
    input: &CompactionInput,
    records: &[AgentRecord],
    reserve_tokens: u64,
    model_max_output: u32,
    model_reasoning: bool,
    base_options: &SimpleGenerationOptions,
    turn_prefix: bool,
) -> Result<ModelRequest, CompactionError> {
    let conversation = serialize_conversation(records)?;
    let previous_summary = input
        .previous_summary
        .as_deref()
        .filter(|summary| !summary.is_empty());
    let custom_instructions = input
        .custom_instructions
        .as_deref()
        .filter(|instructions| !instructions.is_empty());
    let mut instructions = if turn_prefix {
        TURN_PREFIX_SUMMARIZATION_PROMPT.to_owned()
    } else if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if !turn_prefix && let Some(custom) = custom_instructions {
        write!(instructions, "\n\nAdditional focus: {custom}")
            .expect("writing to a String cannot fail");
    }
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if !turn_prefix && let Some(previous) = previous_summary {
        write!(
            prompt,
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        )
        .expect("writing to a String cannot fail");
    }
    prompt.push_str(&instructions);
    let kind = if turn_prefix {
        "turn-prefix"
    } else {
        "history"
    };
    let mut context = Context::new(Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new(format!("{}-{kind}-user", input.result_entry_id)),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{}-{kind}-text", input.result_entry_id)),
            text: prompt,
        }],
        timestamp: input.timestamp,
    }));
    let fraction = if turn_prefix { 1_u64 } else { 4_u64 };
    let denominator = if turn_prefix { 2_u64 } else { 5_u64 };
    let reserve_cap = reserve_tokens
        .saturating_mul(fraction)
        .checked_div(denominator)
        .unwrap_or(0);
    let reserve_output = u32::try_from(reserve_cap).unwrap_or(u32::MAX);
    let max_output = if model_max_output == 0 {
        reserve_output
    } else {
        reserve_output.min(model_max_output)
    };
    let mut options = base_options.clone();
    options.max_output_tokens = Some(max_output);
    options.cache_retention = Some(CacheRetention::None);
    options.session_id = Some(format!("{}-{kind}-session", input.result_entry_id));
    options.reasoning =
        (model_reasoning && input.reasoning != ReasoningLevel::Off).then_some(input.reasoning);
    Ok(ModelRequest {
        model: input.summary_model.clone(),
        context,
        options,
    })
}

fn validate_summary_terminal(
    message: AssistantMessage,
    turn_prefix: bool,
) -> Result<AssistantMessage, CompactionError> {
    match message.finish.reason {
        AssistantFinishReason::Aborted => Err(CompactionError::Cancelled),
        AssistantFinishReason::Error => {
            let prefix = if turn_prefix {
                "Turn prefix summarization failed"
            } else {
                "Summarization failed"
            };
            let detail = message
                .finish
                .error
                .as_ref()
                .map_or("Unknown error", |error| error.message.as_str());
            Err(CompactionError::summarization(format!(
                "{prefix}: {detail}"
            )))
        }
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => Ok(message),
    }
}

fn combine_terminal_messages(
    history: Option<&AssistantMessage>,
    prefix: &AssistantMessage,
) -> AssistantMessage {
    let Some(history) = history else {
        return prefix.clone();
    };
    let mut combined = prefix.clone();
    combined.usage = combine_usage(&history.usage, &prefix.usage);
    combined.cost = match (&history.cost, &prefix.cost) {
        (Some(left), Some(right)) if left.currency == right.currency => Some(pi_ai::Cost {
            currency: left.currency.clone(),
            micros: left.micros.saturating_add(right.micros),
        }),
        _ => None,
    };
    combined
}

fn combine_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        reasoning_tokens: sum_options(left.reasoning_tokens, right.reasoning_tokens),
        cache_read_tokens: sum_options(left.cache_read_tokens, right.cache_read_tokens),
        cache_write_tokens: sum_options(left.cache_write_tokens, right.cache_write_tokens),
        cache_write_one_hour_tokens: sum_options(
            left.cache_write_one_hour_tokens,
            right.cache_write_one_hour_tokens,
        ),
        total_tokens: sum_options(left.total_tokens, right.total_tokens),
        source: if left.source == right.source {
            left.source
        } else {
            UsageSource::Mixed
        },
    }
}

fn sum_options(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    (left.is_some() || right.is_some())
        .then(|| left.unwrap_or(0).saturating_add(right.unwrap_or(0)))
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serializes model-visible records into pinned Pi's summary transcript format.
pub fn serialize_conversation(records: &[AgentRecord]) -> Result<String, CompactionError> {
    let mut parts = Vec::new();
    for (record_index, record) in records.iter().enumerate() {
        let Some(message) = project_record_to_message(record, record_index) else {
            continue;
        };
        match &message {
            Message::User(message) => {
                let text = content_text(&message.content, "");
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(message) => {
                let thinking = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                let text = content_text(&message.content, "\n");
                if message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { .. }))
                {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { call, .. } => Some(call),
                        _ => None,
                    })
                    .map(|call| format!("{}({})", call.name, format_tool_arguments(call)))
                    .collect::<Vec<_>>();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(message) => {
                let text = tool_result_text(&message.content);
                if !text.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_utf16(&text, 2_000)));
                }
            }
        }
    }
    Ok(parts.join("\n\n"))
}

pub(crate) fn project_record_to_message(
    record: &AgentRecord,
    record_index: usize,
) -> Option<Message> {
    match record {
        AgentRecord::Llm(message) => Some(message.clone()),
        AgentRecord::Custom { type_name, payload } if is_bash_execution(type_name) => {
            project_bash_execution(payload, record_index)
        }
        AgentRecord::Custom { type_name, payload } if type_name == CUSTOM_ROLE => {
            Some(project_custom_content(payload, record_index))
        }
        AgentRecord::Custom { type_name, payload } if type_name == BRANCH_SUMMARY_ROLE => {
            Some(project_summary_content(
                payload,
                record_index,
                BRANCH_SUMMARY_PREFIX,
                BRANCH_SUMMARY_SUFFIX,
            ))
        }
        AgentRecord::Custom { type_name, payload } if type_name == COMPACTION_SUMMARY_ROLE => {
            Some(project_summary_content(
                payload,
                record_index,
                COMPACTION_SUMMARY_PREFIX,
                COMPACTION_SUMMARY_SUFFIX,
            ))
        }
        AgentRecord::Custom { .. } => None,
    }
}

pub(crate) fn is_projectable_harness_role(type_name: &str) -> bool {
    matches!(
        type_name,
        BASH_EXECUTION_ROLE | CUSTOM_ROLE | BRANCH_SUMMARY_ROLE | COMPACTION_SUMMARY_ROLE
    )
}

fn is_bash_execution(type_name: &str) -> bool {
    type_name == BASH_EXECUTION_ROLE
}

fn project_bash_execution(payload: &RawValue, record_index: usize) -> Option<Message> {
    let value = serde_json::from_str::<Value>(payload.get()).ok();
    let object = value.as_ref().and_then(Value::as_object);
    if object
        .and_then(|value| {
            value
                .get("excludeFromContext")
                .or_else(|| value.get("exclude_from_context"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let command = string_field(object, &["command"]);
    let output = string_field(object, &["output"]);
    let cancelled = bool_field(object, &["cancelled"]);
    let truncated = bool_field(object, &["truncated"]);
    let full_output_path = string_field(object, &["fullOutputPath", "full_output_path"]);
    let exit_code = object
        .and_then(|value| value.get("exitCode").or_else(|| value.get("exit_code")))
        .and_then(Value::as_i64);

    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        write!(text, "```\n{output}\n```").expect("writing to a String cannot fail");
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = exit_code.filter(|code| *code != 0) {
        write!(text, "\n\nCommand exited with code {exit_code}")
            .expect("writing to a String cannot fail");
    }
    if truncated && !full_output_path.is_empty() {
        write!(
            text,
            "\n\n[Output truncated. Full output: {full_output_path}]"
        )
        .expect("writing to a String cannot fail");
    }
    Some(custom_user_message(
        record_index,
        vec![custom_text_block(record_index, 0, text)],
        custom_timestamp(object),
    ))
}

fn project_custom_content(payload: &RawValue, record_index: usize) -> Message {
    let value = serde_json::from_str::<Value>(payload.get())
        .unwrap_or_else(|_| Value::String(payload.get().to_owned()));
    let object = value.as_object();
    let content = object
        .and_then(|value| value.get("content"))
        .unwrap_or(&value);
    let blocks = custom_content_blocks(content, record_index);
    custom_user_message(record_index, blocks, custom_timestamp(object))
}

fn project_summary_content(
    payload: &RawValue,
    record_index: usize,
    prefix: &str,
    suffix: &str,
) -> Message {
    let value = serde_json::from_str::<Value>(payload.get()).ok();
    let object = value.as_ref().and_then(Value::as_object);
    custom_user_message(
        record_index,
        vec![custom_text_block(
            record_index,
            0,
            format!("{prefix}{}{suffix}", string_field(object, &["summary"])),
        )],
        custom_timestamp(object),
    )
}

pub(crate) fn branch_summary_record(
    summary: &str,
    from_id: &EntryId,
    timestamp: Timestamp,
) -> AgentRecord {
    structural_summary_record(
        BRANCH_SUMMARY_ROLE,
        serde_json::json!({
            "summary": summary,
            "fromId": from_id.as_str(),
            "timestamp": timestamp.unix_millis(),
        }),
    )
}

pub(crate) fn compaction_summary_record(
    summary: &str,
    tokens_before: u64,
    timestamp: Timestamp,
) -> AgentRecord {
    structural_summary_record(
        COMPACTION_SUMMARY_ROLE,
        serde_json::json!({
            "summary": summary,
            "tokensBefore": tokens_before,
            "timestamp": timestamp.unix_millis(),
        }),
    )
}

fn structural_summary_record(type_name: &str, payload: Value) -> AgentRecord {
    AgentRecord::Custom {
        type_name: type_name.to_owned(),
        payload: serde_json::value::to_raw_value(&payload)
            .expect("structural summary payload is JSON-serializable"),
    }
}

fn custom_content_blocks(content: &Value, record_index: usize) -> Vec<ContentBlock> {
    match content {
        Value::String(text) => vec![custom_text_block(record_index, 0, text.clone())],
        Value::Array(values) => {
            let blocks = values
                .iter()
                .enumerate()
                .filter_map(|(block_index, value)| {
                    let object = value.as_object()?;
                    match object.get("type").and_then(Value::as_str) {
                        Some("text") => Some(custom_text_block(
                            record_index,
                            block_index,
                            string_field(Some(object), &["text"]),
                        )),
                        Some("image") => Some(ContentBlock::Image {
                            id: custom_block_id(record_index, block_index),
                            data: string_field(Some(object), &["data"]),
                            mime_type: string_field(Some(object), &["mimeType", "mime_type"]),
                        }),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            if blocks.is_empty() && !values.is_empty() {
                vec![custom_text_block(record_index, 0, content.to_string())]
            } else {
                blocks
            }
        }
        Value::Null => Vec::new(),
        Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
            vec![custom_text_block(record_index, 0, content.to_string())]
        }
    }
}

fn custom_user_message(
    record_index: usize,
    content: Vec<ContentBlock>,
    timestamp: Timestamp,
) -> Message {
    Message::User(UserMessage {
        id: MessageId::new(format!("harness-custom-{record_index}")),
        content,
        timestamp,
    })
}

fn custom_text_block(record_index: usize, block_index: usize, text: String) -> ContentBlock {
    ContentBlock::Text {
        id: custom_block_id(record_index, block_index),
        text,
    }
}

fn custom_block_id(record_index: usize, block_index: usize) -> ContentBlockId {
    ContentBlockId::new(format!("harness-custom-{record_index}-block-{block_index}"))
}

fn custom_timestamp(object: Option<&Map<String, Value>>) -> Timestamp {
    object
        .and_then(|value| value.get("timestamp"))
        .and_then(Value::as_i64)
        .map_or_else(Timestamp::default, Timestamp::from_unix_millis)
}

fn string_field(object: Option<&Map<String, Value>>, names: &[&str]) -> String {
    object
        .and_then(|value| names.iter().find_map(|name| value.get(*name)))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn bool_field(object: Option<&Map<String, Value>>, names: &[&str]) -> bool {
    object
        .and_then(|value| names.iter().find_map(|name| value.get(*name)))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn format_tool_arguments(call: &pi_ai::ToolCall) -> String {
    let serde_json::Value::Object(arguments) = &call.arguments else {
        return String::new();
    };
    arguments
        .iter()
        .map(|(key, value)| {
            let value = pi_ai::json_stringify_compatible(value)
                .unwrap_or_else(|_| "[unserializable]".to_owned());
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn content_text(content: &[ContentBlock], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn tool_result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text { text, .. } => Some(text.as_str()),
            ToolResultContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn truncate_utf16(text: &str, max_units: usize) -> String {
    let total = text.encode_utf16().count();
    if total <= max_units {
        return text.to_owned();
    }
    let mut used = 0_usize;
    let end = text
        .char_indices()
        .take_while(|(_, character)| {
            let units = character.len_utf16();
            let keep = used.saturating_add(units) <= max_units;
            if keep {
                used += units;
            }
            keep
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    format!(
        "{}\n\n[... {} more characters truncated]",
        &text[..end],
        total.saturating_sub(used)
    )
}

/// Returns the latest compaction summary on a chronological branch.
pub fn latest_compaction_summary(entries: &[SessionEntry]) -> Option<String> {
    entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction { summary, .. } => Some(summary.clone()),
        _ => None,
    })
}
