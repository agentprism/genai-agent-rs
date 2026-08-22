//! Context projection and post-turn policy seams from Architecture v2 part 1
//! §4.7–§4.8 and part 2 §2.2, §4.4, and §8.1.

use crate::{AgentError, AgentRecord, AgentState, ToolOutput, TurnOutcome};
use pi_ai::{
    ApiId, AssistantFinishReason, AssistantMessage, CancellationToken, Context, HandoffChange,
    HandoffReport, LocalBoxFuture, Message, ModelFingerprint, ModelRef, ReasoningLevel,
    SendBoxFuture, SimpleGenerationOptions, ToolCall, ToolResultContent, ToolResultMessage,
    ToolSpec, Usage,
};
use serde_json::{Value, value::RawValue};
use std::fmt;

/// Borrowed durable and run-local state supplied to context preparation.
pub struct AgentStateView<'a> {
    /// Durable configured state. `transcript` remains the authoritative history.
    pub state: &'a AgentState,
    /// Current run-local record view after any `prepare_next_turn` replacement.
    pub records: &'a [AgentRecord],
    /// Model-facing specifications of currently bound executable tools.
    pub tools: &'a [ToolSpec],
    /// Model selected for this request, including a run-local override.
    pub model: &'a ModelRef,
    /// Reasoning selected for this request, including a run-local override.
    pub reasoning: ReasoningLevel,
}

/// Complete provider-neutral request context prepared for one model turn.
pub struct PreparedContext {
    /// Projected canonical context.
    pub context: Context,
    /// Optional model replacement for this request and later turns in the run.
    pub model_override: Option<ModelRef>,
    /// Optional common generation-option replacement.
    pub options_override: Option<SimpleGenerationOptions>,
    /// Structured projection and handoff diagnostics.
    pub report: HandoffReport,
}

/// Failure while preparing the provider-visible context.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextError {
    /// Preparation observed cancellation.
    Cancelled,
    /// A policy rejected or could not project the context.
    Projection {
        /// Sanitized policy diagnostic.
        message: String,
    },
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("context preparation was cancelled"),
            Self::Projection { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ContextError {}

/// Thread-safe context preparation and failed-turn projection policy.
pub trait ContextPolicy: Send + Sync + 'static {
    /// Prepares one provider-neutral context immediately before the model call.
    fn prepare<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedContext, ContextError>>;
}

/// Local-executor context preparation policy.
pub trait LocalContextPolicy: 'static {
    /// Prepares one provider-neutral context without requiring `Send`.
    fn prepare<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedContext, ContextError>>;
}

/// Pi-compatible default context policy.
///
/// Custom records remain UI-only and failed or aborted assistants remain
/// durable while being omitted from provider projection (part 2 §2.2).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultContextPolicy;

impl ContextPolicy for DefaultContextPolicy {
    fn prepare<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedContext, ContextError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ContextError::Cancelled);
            }
            Ok(project_default_context(state))
        })
    }
}

impl LocalContextPolicy for DefaultContextPolicy {
    fn prepare<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedContext, ContextError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ContextError::Cancelled);
            }
            Ok(project_default_context(state))
        })
    }
}

fn project_default_context(state: AgentStateView<'_>) -> PreparedContext {
    let target_api = state
        .records
        .iter()
        .rev()
        .find_map(|record| match record {
            AgentRecord::Llm(Message::Assistant(message))
                if message.provider == state.model.provider
                    && message.requested_model == state.model.model =>
            {
                Some(message.api.clone())
            }
            AgentRecord::Llm(_) | AgentRecord::Custom { .. } => None,
        })
        .unwrap_or_else(|| ApiId::new("unknown"));
    let target = ModelFingerprint::new(
        state.model.provider.clone(),
        target_api,
        state.model.model.clone(),
    );
    let mut report = HandoffReport::unchanged(target);
    let mut messages = Vec::new();

    for record in state.records {
        let AgentRecord::Llm(message) = record else {
            continue;
        };
        if let Message::Assistant(assistant) = message {
            report.source_models.insert(ModelFingerprint::new(
                assistant.provider.clone(),
                assistant.api.clone(),
                assistant
                    .response_model
                    .clone()
                    .unwrap_or_else(|| assistant.requested_model.clone()),
            ));
            if matches!(
                assistant.finish.reason,
                AssistantFinishReason::Error | AssistantFinishReason::Aborted
            ) {
                report.changes.push(HandoffChange::FailedAssistantOmitted {
                    message_id: assistant.id.clone(),
                    reason: assistant.finish.reason,
                });
                report.lossy = true;
                continue;
            }
        }
        messages.push(message.clone());
    }

    PreparedContext {
        context: Context {
            schema_version: pi_ai::CONTEXT_SCHEMA_VERSION,
            system_prompt: (!state.state.system_prompt.is_empty())
                .then(|| state.state.system_prompt.clone()),
            messages,
            tools: state.tools.to_vec(),
        },
        model_override: None,
        options_override: None,
        report,
    }
}

/// Deterministic pre-execution authorization selected by [`ToolPolicy`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ToolAuthorization {
    /// Permit the validated call to execute.
    #[default]
    Allow,
    /// Produce an error result without invoking the tool.
    Block {
        /// Optional model-visible reason; a Pi-compatible default is used when
        /// absent.
        reason: Option<String>,
        /// Termination hint applied to the blocked result.
        terminate: bool,
    },
}

/// Borrowed inputs to deterministic tool preflight.
pub struct BeforeToolCall<'a> {
    /// Assistant message that requested the complete batch.
    pub assistant_message: &'a AssistantMessage,
    /// Source call exactly as committed in the assistant message.
    pub tool_call: &'a ToolCall,
    /// Prepared and JSON-Schema-validated arguments. Pi-compatible policies
    /// may mutate this value after validation; execution observes the mutation
    /// without a second validation pass.
    pub args: &'a mut Value,
    /// Current run-local record view.
    pub records: &'a [AgentRecord],
}

/// Borrowed inputs to post-execution tool finalization.
pub struct AfterToolCall<'a> {
    /// Assistant message that requested the complete batch.
    pub assistant_message: &'a AssistantMessage,
    /// Source call exactly as committed in the assistant message.
    pub tool_call: &'a ToolCall,
    /// Prepared and JSON-Schema-validated arguments used for execution.
    pub args: &'a Value,
    /// Tool output before post-execution replacement fields are applied.
    pub result: &'a ToolOutput,
    /// Whether the result currently represents an execution failure.
    pub is_error: bool,
    /// Current run-local record view.
    pub records: &'a [AgentRecord],
}

/// Field-by-field replacement returned by [`ToolPolicy::finalize`].
///
/// `None` retains the executed value. Provided content, details, and usage
/// replace their complete fields rather than being deep-merged, matching Pi's
/// `afterToolCall` contract.
#[derive(Clone, Debug, Default)]
pub struct ToolOutputPatch {
    /// Replacement model-visible content.
    pub content: Option<Vec<ToolResultContent>>,
    /// Replacement tool-owned details.
    pub details: Option<Box<RawValue>>,
    /// Replacement tool usage.
    pub usage: Option<Usage>,
    /// Replacement error classification.
    pub is_error: Option<bool>,
    /// Replacement termination hint.
    pub terminate: Option<bool>,
}

impl ToolOutputPatch {
    pub(crate) fn apply(self, mut output: ToolOutput, mut is_error: bool) -> (ToolOutput, bool) {
        if let Some(content) = self.content {
            output.content = content;
        }
        if let Some(details) = self.details {
            output.details = Some(details);
        }
        if let Some(usage) = self.usage {
            output.usage = Some(usage);
        }
        if let Some(terminate) = self.terminate {
            output.terminate = terminate;
        }
        if let Some(replacement) = self.is_error {
            is_error = replacement;
        }
        (output, is_error)
    }
}

/// Thread-safe authorization and finalization policy for executable tools.
pub trait ToolPolicy: Send + Sync + 'static {
    /// Authorizes one normalized and validated call during source-ordered
    /// preflight.
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, AgentError>>;

    /// Finalizes one executed output before completion events and transcript
    /// messages are produced.
    fn finalize<'a>(
        &'a self,
        context: AfterToolCall<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, AgentError>>;
}

/// Local-executor authorization and finalization policy.
pub trait LocalToolPolicy: 'static {
    /// Authorizes one local normalized and validated call.
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>>;

    /// Finalizes one local executed output.
    fn finalize<'a>(
        &'a self,
        context: AfterToolCall<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, AgentError>>;
}

/// Pi-compatible allow/no-op tool policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultToolPolicy;

impl ToolPolicy for DefaultToolPolicy {
    fn authorize<'a>(
        &'a self,
        _context: BeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async { Ok(ToolAuthorization::Allow) })
    }

    fn finalize<'a>(
        &'a self,
        _context: AfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        Box::pin(async { Ok(ToolOutputPatch::default()) })
    }
}

impl LocalToolPolicy for DefaultToolPolicy {
    fn authorize<'a>(
        &'a self,
        _context: BeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async { Ok(ToolAuthorization::Allow) })
    }

    fn finalize<'a>(
        &'a self,
        _context: AfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        Box::pin(async { Ok(ToolOutputPatch::default()) })
    }
}

/// Immutable facts available after `TurnFinished` and before queue polling.
pub struct CompletedTurn<'a> {
    /// Committed turn outcome.
    pub outcome: &'a TurnOutcome,
    /// Committed assistant message.
    pub assistant: &'a AssistantMessage,
    /// Committed tool results in assistant source order.
    pub tool_results: &'a [ToolResultMessage],
    /// Current run-local record view.
    pub records: &'a [AgentRecord],
}

/// Run-local replacements selected after a completed turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NextTurn {
    /// Replacement record view for subsequent context preparation.
    pub records: Option<Vec<AgentRecord>>,
    /// Replacement model for subsequent requests in this run.
    pub model: Option<ModelRef>,
    /// Replacement reasoning level for subsequent requests in this run.
    pub reasoning: Option<ReasoningLevel>,
}

/// Failure from post-turn policy evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnPolicyError {
    /// Sanitized policy diagnostic.
    pub message: String,
}

impl TurnPolicyError {
    /// Creates a post-turn policy error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TurnPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TurnPolicyError {}

/// Thread-safe policy evaluated after each complete turn.
pub trait TurnPolicy: Send + Sync + 'static {
    /// Optionally replaces run-local context, model, or reasoning.
    fn prepare_next_turn<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>>;

    /// Decides whether to stop before either queue is polled.
    fn should_stop<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>>;
}

/// Local-executor post-turn policy.
pub trait LocalTurnPolicy: 'static {
    /// Optionally replaces run-local context, model, or reasoning.
    fn prepare_next_turn<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<NextTurn, TurnPolicyError>>;

    /// Decides whether to stop before either queue is polled.
    fn should_stop<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<bool, TurnPolicyError>>;
}

/// No-op post-turn policy used by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultTurnPolicy;

impl TurnPolicy for DefaultTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async { Ok(NextTurn::default()) })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async { Ok(false) })
    }
}

impl LocalTurnPolicy for DefaultTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async { Ok(NextTurn::default()) })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async { Ok(false) })
    }
}
