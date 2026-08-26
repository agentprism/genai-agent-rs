//! Context projection and post-turn policy seams from Architecture v2 part 1
//! §4.7–§4.8 and part 2 §2.2, §4.4, and §8.1.

use crate::{
    AgentError, AgentRecord, AgentState, LocalToolRegistry, ToolOutput, ToolRegistry, TurnOutcome,
};
use agentprism_ai::{
    AssistantMessage, CancellationToken, Context, HandoffReport, LocalBoxFuture, Message, ModelRef,
    ReasoningLevel, SendBoxFuture, SimpleGenerationOptions, ToolCall, ToolResultContent,
    ToolResultMessage, ToolSpec, Usage,
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
    /// Complete configured common generation options for this request.
    ///
    /// Policies that change one option must clone this value before producing
    /// the complete replacement carried by `options_override`.
    pub options: &'a SimpleGenerationOptions,
}

/// Complete run-local agent context used between model turns.
///
/// The executable-tool parameter keeps the Send and Local runtime families
/// equally expressive: [`AgentContext`] owns a [`ToolRegistry`], while
/// [`LocalAgentContext`] owns a [`LocalToolRegistry`]. Replacing this value is
/// the Rust equivalent of Pi replacing its complete `AgentContext`, including
/// `systemPrompt`, `messages`, and executable tools.
#[derive(Clone, Debug)]
pub struct AgentRunContext<Tools> {
    /// System instruction used for subsequent requests in this run.
    pub system_prompt: String,
    /// Agent records visible to subsequent context-policy preparation.
    pub records: Vec<AgentRecord>,
    /// Executable tools active for subsequent requests and tool batches.
    pub tools: Tools,
}

/// Complete Send-runtime context used within one run.
pub type AgentContext = AgentRunContext<ToolRegistry>;

/// Complete Local-runtime context used within one run.
pub type LocalAgentContext = AgentRunContext<LocalToolRegistry>;

/// Agent-record preparation produced before message projection.
///
/// This intermediate value keeps Pi's `transformContext` and `convertToLlm`
/// phases distinct while retaining Architecture v2's model and option
/// override capabilities. The core turns it into [`PreparedContext`] only
/// after [`MessageProjector::project`] succeeds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedAgentRecords {
    /// Run-local records supplied to message projection.
    pub records: Vec<AgentRecord>,
    /// Optional model replacement for this request and later turns in the run.
    pub model_override: Option<ModelRef>,
    /// Optional complete common generation-option replacement.
    pub options_override: Option<SimpleGenerationOptions>,
    /// Target-aware handoff report produced by a context policy that performs
    /// canonical projection itself. It is propagated to [`PreparedContext`]
    /// after message projection and surfaced by the run loop.
    pub report: Option<HandoffReport>,
}

/// Complete provider-neutral request context prepared for one model turn.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedContext {
    /// Projected canonical context.
    pub context: Context,
    /// Optional model replacement for this request and later turns in the run.
    pub model_override: Option<ModelRef>,
    /// Optional common generation-option replacement.
    pub options_override: Option<SimpleGenerationOptions>,
    /// Handoff report surfaced for this prepared request. A target-aware policy
    /// supplies the resolved API fingerprint; the built-in projection supplies
    /// an unchanged report with the explicit `provider-neutral` API identity.
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

/// Thread-safe Agent-record preparation policy.
///
/// This is the Rust mapping of Pi's `transformContext` phase. It deliberately
/// runs before [`MessageProjector`] and does not perform provider/API handoff;
/// that later layer owns its [`agentprism_ai::HandoffReport`].
pub trait ContextPolicy: Send + Sync + 'static {
    /// Prepares run-local Agent records immediately before message projection.
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>>;
}

/// Local-executor context preparation policy.
pub trait LocalContextPolicy: 'static {
    /// Prepares local Agent records without requiring `Send`.
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedAgentRecords, ContextError>>;
}

/// Pi-compatible identity `transformContext` policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultContextPolicy;

impl ContextPolicy for DefaultContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ContextError::Cancelled);
            }
            Ok(PreparedAgentRecords {
                records: state.records.to_vec(),
                model_override: None,
                options_override: None,
                report: None,
            })
        })
    }
}

impl LocalContextPolicy for DefaultContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ContextError::Cancelled);
            }
            Ok(PreparedAgentRecords {
                records: state.records.to_vec(),
                model_override: None,
                options_override: None,
                report: None,
            })
        })
    }
}

/// Thread-safe projection from extensible Agent records to canonical messages.
///
/// This is the Rust mapping of Pi's `convertToLlm`. It is called exactly once
/// per model turn, after [`ContextPolicy::prepare_agent_records`].
pub trait MessageProjector: Send + Sync + 'static {
    /// Projects prepared Agent records into provider-neutral LLM messages.
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> SendBoxFuture<'a, Result<Vec<Message>, ContextError>>;
}

/// Local-executor message projector.
pub trait LocalMessageProjector: 'static {
    /// Projects prepared local Agent records without requiring `Send`.
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> LocalBoxFuture<'a, Result<Vec<Message>, ContextError>>;
}

/// Pi-compatible default `convertToLlm` projection.
///
/// Canonical user, assistant, and tool-result records pass through unchanged;
/// custom records remain durable and UI-visible but are omitted from model
/// context. Provider/API handoff may subsequently omit failed assistants.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultMessageProjector;

impl MessageProjector for DefaultMessageProjector {
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> SendBoxFuture<'a, Result<Vec<Message>, ContextError>> {
        Box::pin(async move { Ok(project_default_messages(records)) })
    }
}

impl LocalMessageProjector for DefaultMessageProjector {
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> LocalBoxFuture<'a, Result<Vec<Message>, ContextError>> {
        Box::pin(async move { Ok(project_default_messages(records)) })
    }
}

fn project_default_messages(records: &[AgentRecord]) -> Vec<Message> {
    records
        .iter()
        .filter_map(|record| match record {
            AgentRecord::Llm(message) => Some(message.clone()),
            AgentRecord::Custom { .. } => None,
        })
        .collect()
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
pub struct BeforeToolCall<'a, Tools = ToolRegistry> {
    /// Assistant message that requested the complete batch.
    pub assistant_message: &'a AssistantMessage,
    /// Source call exactly as committed in the assistant message.
    pub tool_call: &'a ToolCall,
    /// Prepared and JSON-Schema-validated arguments. Pi-compatible policies
    /// may mutate this value after validation; execution observes the mutation
    /// without a second validation pass.
    pub args: &'a mut Value,
    /// Complete current run-local context, including system prompt, records,
    /// and executable tools.
    pub context: &'a AgentRunContext<Tools>,
}

/// Borrowed inputs to post-execution tool finalization.
pub struct AfterToolCall<'a, Tools = ToolRegistry> {
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
    /// Complete current run-local context, including system prompt, records,
    /// and executable tools.
    pub context: &'a AgentRunContext<Tools>,
}

/// Local-runtime inputs to deterministic tool preflight.
pub type LocalBeforeToolCall<'a> = BeforeToolCall<'a, LocalToolRegistry>;

/// Local-runtime inputs to post-execution tool finalization.
pub type LocalAfterToolCall<'a> = AfterToolCall<'a, LocalToolRegistry>;

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
///
/// Authorization here is a logical allow/block decision, not an operating
/// system sandbox or security boundary. Isolation remains the responsibility
/// of the tool implementation and its host environment.
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
///
/// Like [`ToolPolicy`], this is not a sandbox or security boundary.
pub trait LocalToolPolicy: 'static {
    /// Authorizes one local normalized and validated call.
    fn authorize<'a>(
        &'a self,
        context: LocalBeforeToolCall<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>>;

    /// Finalizes one local executed output.
    fn finalize<'a>(
        &'a self,
        context: LocalAfterToolCall<'a>,
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
        _context: LocalBeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async { Ok(ToolAuthorization::Allow) })
    }

    fn finalize<'a>(
        &'a self,
        _context: LocalAfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        Box::pin(async { Ok(ToolOutputPatch::default()) })
    }
}

/// Immutable facts available after `TurnFinished` and before queue polling.
pub struct CompletedTurn<'a, Tools = ToolRegistry> {
    /// Committed turn outcome.
    pub outcome: &'a TurnOutcome,
    /// Committed assistant message.
    pub assistant: &'a AssistantMessage,
    /// Committed tool results in assistant source order.
    pub tool_results: &'a [ToolResultMessage],
    /// Complete current run-local context.
    pub context: &'a AgentRunContext<Tools>,
    /// Records committed by this loop invocation so far. Prompt runs include
    /// their initial prompt records; continuation and retry runs exclude
    /// records that predate the invocation. This accumulator is independent
    /// of replaceable [`Self::context`].
    pub new_messages: &'a [AgentRecord],
}

/// Run-local replacements selected after a completed turn.
#[derive(Clone, Debug)]
pub struct NextTurn<Tools = ToolRegistry> {
    /// Replacement complete context for subsequent preparation, requests, and
    /// tool execution within this run.
    pub context: Option<AgentRunContext<Tools>>,
    /// Replacement model for subsequent requests in this run.
    pub model: Option<ModelRef>,
    /// Replacement reasoning level for subsequent requests in this run.
    pub reasoning: Option<ReasoningLevel>,
}

impl<Tools> Default for NextTurn<Tools> {
    fn default() -> Self {
        Self {
            context: None,
            model: None,
            reasoning: None,
        }
    }
}

/// Completed-turn input for the Local runtime family.
pub type LocalCompletedTurn<'a> = CompletedTurn<'a, LocalToolRegistry>;

/// Next-turn replacement for the Local runtime family.
pub type LocalNextTurn = NextTurn<LocalToolRegistry>;

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
        turn: LocalCompletedTurn<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<LocalNextTurn, TurnPolicyError>>;

    /// Decides whether to stop before either queue is polled.
    fn should_stop<'a>(
        &'a self,
        turn: LocalCompletedTurn<'a>,
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
        _turn: LocalCompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<LocalNextTurn, TurnPolicyError>> {
        Box::pin(async { Ok(LocalNextTurn::default()) })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: LocalCompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async { Ok(false) })
    }
}
