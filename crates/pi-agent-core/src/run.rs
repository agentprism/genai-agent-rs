//! Borrowed agent run streams, phases, queue polling, continuation, retry, and
//! reset behavior from Architecture v2 part 1 §4.3 and part 2 §2.1–§2.3,
//! §8, and §9.

use crate::{
    AgentContext, AgentControl, AgentError, AgentEvent, AgentRecord, AgentRunContext, AgentState,
    AgentStateView, CompletedTurn, ContextError, ContextPolicy, DefaultContextPolicy,
    DefaultMessageProjector, DefaultTurnPolicy, LocalAgent, LocalAgentContext, LocalContextPolicy,
    LocalMessageProjector, LocalToolPolicy, LocalToolRegistry, LocalToolScheduler, LocalTurnPolicy,
    MessageProjector, MessageRole, NextTurn, PreparedContext, QueueKind, RunOutcome,
    ToolBatchRequest, ToolBatchStreamEvent, ToolCallOutcome, ToolExecutionMode, ToolPolicy,
    ToolRegistry, ToolScheduler, TurnOutcome, TurnPolicy,
};
use futures_util::StreamExt;
use pi_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    AssistantMessage, AssistantMessageSnapshot, CancellationReason, CancellationToken,
    ContentBlock, ContentBlockId, Context, LocalBoxStream, Message, MessageId, ModelRequest,
    ModelRuntime, PublicError, ReplayCompleteness, ReplayEnvelope, ReplayScope, RequestStartError,
    RequestStartErrorKind, RunId, SendBoxStream, SimpleGenerationOptions, Timestamp, ToolCallId,
    ToolResultMessage, Usage, UsageSource, UserMessage, VersionedExtension,
};
use serde::{Deserialize, Serialize};
use std::{rc::Rc, sync::Arc};

/// Explicit state-machine phases. Queue receivers are polled only in the
/// phases whose names say so.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// Allocate and announce a run.
    StartRun,
    /// Poll steering after initial prompt commitment and before the first
    /// model request.
    InitialQueuePoll,
    /// Commit prompt or drained queue records.
    InjectPendingMessages,
    /// Project durable records into provider-neutral context.
    PrepareContext,
    /// Establish and consume one assistant stream.
    RequestAssistant,
    /// Commit the terminal assistant record.
    CommitAssistant,
    /// Resolve one complete assistant tool-call batch.
    PrepareToolBatch,
    /// Execute the complete tool batch without queue polling.
    ExecuteToolBatch,
    /// Commit source-ordered tool results.
    CommitToolResults,
    /// Emit the completed turn outcome.
    FinishTurn,
    /// Apply run-local context/model/reasoning replacement.
    PrepareNextTurn,
    /// Decide whether to stop before queue polling.
    ShouldStopAfterTurn,
    /// Poll the steering queue after the whole turn and policies.
    PollSteering,
    /// Mark the point at which no automatic tool continuation remains.
    WouldStop,
    /// Poll follow-up only when the agent would otherwise stop.
    PollFollowUp,
    /// Emit the run's final event.
    FinishRun,
}

/// Records supplied to [`crate::Agent::run`] as a new prompt batch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentInput {
    /// Records committed and emitted in order before the first assistant call.
    pub records: Vec<AgentRecord>,
}

impl AgentInput {
    /// Creates a prompt input from an ordered record batch.
    pub fn records(records: impl IntoIterator<Item = AgentRecord>) -> Self {
        Self {
            records: records.into_iter().collect(),
        }
    }
}

/// Image supplied by the text-prompt convenience surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptImage {
    /// Base64 image bytes without a data-URL prefix.
    pub data: String,
    /// Image media type such as `image/png`.
    pub mime_type: String,
}

/// Text followed by zero or more images, matching Pi's prompt construction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PromptText {
    /// Leading user text block.
    pub text: String,
    /// Images appended after the text block in caller order.
    pub images: Vec<PromptImage>,
}

#[derive(Clone)]
pub(crate) struct AgentDefaults {
    system_prompt: String,
    model: pi_ai::ModelRef,
    reasoning: pi_ai::ReasoningLevel,
    tools: ToolRegistry,
}

impl AgentDefaults {
    pub(crate) fn new(state: &AgentState, tools: &ToolRegistry) -> Self {
        Self {
            system_prompt: state.system_prompt.clone(),
            model: state.model.clone(),
            reasoning: state.reasoning,
            tools: tools.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct LocalAgentDefaults {
    system_prompt: String,
    model: pi_ai::ModelRef,
    reasoning: pi_ai::ReasoningLevel,
    tools: LocalToolRegistry,
}

impl LocalAgentDefaults {
    pub(crate) fn new(state: &AgentState, tools: &LocalToolRegistry) -> Self {
        Self {
            system_prompt: state.system_prompt.clone(),
            model: state.model.clone(),
            reasoning: state.reasoning,
            tools: tools.clone(),
        }
    }
}

impl crate::Agent {
    /// Creates an idle agent around an explicit narrow runtime capability.
    pub fn new(
        runtime: Arc<dyn ModelRuntime>,
        state: AgentState,
        tools: ToolRegistry,
    ) -> Result<Self, AgentError> {
        tools.validate()?;
        let (control, queue_rx) = AgentControl::channel(crate::DEFAULT_QUEUE_CAPACITY);
        let defaults = AgentDefaults::new(&state, &tools);
        Ok(Self {
            runtime,
            state,
            tools,
            next_sequence: crate::AGENT_INITIAL_SEQUENCE,
            streaming: None,
            pending_tool_calls: Arc::from([]),
            control,
            queue_rx,
            active_run: None,
            phase: None,
            last_error: None,
            options: SimpleGenerationOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            tool_scheduler: ToolScheduler::default(),
            context_policy: Arc::new(DefaultContextPolicy),
            message_projector: Arc::new(DefaultMessageProjector),
            turn_policy: Arc::new(DefaultTurnPolicy),
            defaults,
            next_identity: 1,
        })
    }

    /// Returns a cloneable handle for steering, follow-up, and cancellation.
    pub fn control(&self) -> AgentControl {
        self.control.clone()
    }

    /// Sets how many steering commands are drained at each steering poll.
    pub fn set_steering_mode(&self, mode: crate::QueueDrainMode) {
        self.control.set_steering_mode(mode);
    }

    /// Returns the steering queue's independent drain mode.
    pub fn steering_mode(&self) -> crate::QueueDrainMode {
        self.control.steering_mode()
    }

    /// Sets how many follow-up commands are drained when the agent would stop.
    pub fn set_follow_up_mode(&self, mode: crate::QueueDrainMode) {
        self.control.set_follow_up_mode(mode);
    }

    /// Returns the follow-up queue's independent drain mode.
    pub fn follow_up_mode(&self) -> crate::QueueDrainMode {
        self.control.follow_up_mode()
    }

    /// Clears queued steering records.
    pub fn clear_steering_queue(&self) -> usize {
        self.control.clear_steering()
    }

    /// Clears queued follow-up records.
    pub fn clear_follow_up_queue(&self) -> usize {
        self.control.clear_follow_up()
    }

    /// Clears both semantic queues.
    pub fn clear_all_queues(&self) -> usize {
        self.control.clear_all()
    }

    /// Returns the active run identity, if a run stream currently owns the agent.
    pub fn active_run_id(&self) -> Option<&RunId> {
        self.active_run.as_ref()
    }

    /// Returns the current transient phase.
    pub fn phase(&self) -> Option<AgentPhase> {
        self.phase
    }

    /// Returns the last failed assistant's structured error.
    pub fn last_error(&self) -> Option<&PublicError> {
        self.last_error.as_ref()
    }

    /// Returns mutable durable configuration and transcript state while idle.
    pub fn state_mut(&mut self) -> Result<&mut AgentState, AgentError> {
        self.require_idle()?;
        Ok(&mut self.state)
    }

    /// Returns common model options retained across transcript reset.
    pub fn options(&self) -> &SimpleGenerationOptions {
        &self.options
    }

    /// Mutates common model options while idle.
    pub fn options_mut(&mut self) -> Result<&mut SimpleGenerationOptions, AgentError> {
        self.require_idle()?;
        Ok(&mut self.options)
    }

    /// Replaces the context policy while retaining explicit runtime injection.
    pub fn set_context_policy(&mut self, policy: Arc<dyn ContextPolicy>) -> Result<(), AgentError> {
        self.require_idle()?;
        self.context_policy = policy;
        Ok(())
    }

    /// Replaces the Agent-record to canonical-message projector.
    pub fn set_message_projector(
        &mut self,
        projector: Arc<dyn MessageProjector>,
    ) -> Result<(), AgentError> {
        self.require_idle()?;
        self.message_projector = projector;
        Ok(())
    }

    /// Replaces the post-turn policy.
    pub fn set_turn_policy(&mut self, policy: Arc<dyn TurnPolicy>) -> Result<(), AgentError> {
        self.require_idle()?;
        self.turn_policy = policy;
        Ok(())
    }

    /// Sets batch scheduling behavior. A sequential tool still overrides this.
    pub fn set_tool_execution_mode(&mut self, mode: ToolExecutionMode) -> Result<(), AgentError> {
        self.require_idle()?;
        self.tool_execution = mode;
        Ok(())
    }

    /// Replaces deterministic tool authorization and finalization policy.
    pub fn set_tool_policy(&mut self, policy: Arc<dyn ToolPolicy>) -> Result<(), AgentError> {
        self.require_idle()?;
        self.tool_scheduler = ToolScheduler::new(policy);
        Ok(())
    }

    /// Starts one prompt run. The returned stream borrows the agent and applies
    /// backpressure at every emitted event.
    pub fn run<'a>(
        &'a mut self,
        input: AgentInput,
        cancellation: CancellationToken,
    ) -> SendBoxStream<'a, AgentEvent> {
        self.start_run(input.records, true, cancellation)
    }

    /// Constructs one user record whose text precedes its images, then runs it.
    pub fn prompt_text<'a>(
        &'a mut self,
        prompt: PromptText,
        cancellation: CancellationToken,
    ) -> SendBoxStream<'a, AgentEvent> {
        let record = self.make_prompt_text(prompt);
        self.start_run(vec![record], true, cancellation)
    }

    /// Runs an already identified prompt record batch.
    pub fn prompt_records<'a>(
        &'a mut self,
        records: impl IntoIterator<Item = AgentRecord>,
        cancellation: CancellationToken,
    ) -> SendBoxStream<'a, AgentEvent> {
        self.start_run(records.into_iter().collect(), true, cancellation)
    }

    /// Continues only from a non-assistant tail, except that an assistant tail
    /// first drains steering and then follow-up exactly as Pi does.
    pub fn continue_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<SendBoxStream<'a, AgentEvent>, AgentError> {
        self.require_idle()?;
        let Some(tail) = self.state.transcript.last() else {
            return Err(AgentError::ContinueWithoutMessages);
        };
        if matches!(tail, AgentRecord::Llm(Message::Assistant(_))) {
            let (steering, follow_up) = self.queue_rx.drain_continue_tail();
            if !steering.is_empty() {
                return Ok(self.start_run(commands_to_records(steering), false, cancellation));
            }
            if !follow_up.is_empty() {
                return Ok(self.start_run(commands_to_records(follow_up), true, cancellation));
            }
            return Err(AgentError::ContinueFromAssistant);
        }
        Ok(self.start_run(Vec::new(), true, cancellation))
    }

    /// Retries the request boundary preceding an errored or aborted assistant.
    /// The failed record stays durable while the retry's run-local context
    /// starts at the last valid request boundary.
    pub fn retry_last_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<SendBoxStream<'a, AgentEvent>, AgentError> {
        self.require_idle()?;
        if !tail_is_failed_assistant(&self.state.transcript) {
            return Err(AgentError::RetryRequiresFailedAssistant);
        }
        let retry_records = self.state.transcript[..self.state.transcript.len() - 1].to_vec();
        Ok(self.start_run_with_context(Vec::new(), true, cancellation, Some(retry_records)))
    }

    /// Clears transcript and all run scratch while retaining configured model,
    /// system prompt, tools, runtime, policies, options, and queue modes.
    pub fn reset_transcript(&mut self) -> Result<(), AgentError> {
        self.require_idle()?;
        self.state.transcript.clear();
        self.streaming = None;
        self.pending_tool_calls = Arc::from([]);
        self.last_error = None;
        self.phase = None;
        self.queue_rx.clear_all();
        Ok(())
    }

    /// Restores builder-time state and tools, resets options and queue modes,
    /// and clears all transcript/run scratch.
    pub fn reset_all(&mut self) -> Result<(), AgentError> {
        self.reset_transcript()?;
        self.state.system_prompt = self.defaults.system_prompt.clone();
        self.state.model = self.defaults.model.clone();
        self.state.reasoning = self.defaults.reasoning;
        self.tools = self.defaults.tools.clone();
        self.options = SimpleGenerationOptions::default();
        self.tool_execution = ToolExecutionMode::Parallel;
        self.tool_scheduler = ToolScheduler::default();
        self.control.set_steering_mode(crate::QueueDrainMode::One);
        self.control.set_follow_up_mode(crate::QueueDrainMode::One);
        self.context_policy = Arc::new(DefaultContextPolicy);
        self.message_projector = Arc::new(DefaultMessageProjector);
        self.turn_policy = Arc::new(DefaultTurnPolicy);
        Ok(())
    }

    fn start_run<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        poll_initial_steering: bool,
        cancellation: CancellationToken,
    ) -> SendBoxStream<'a, AgentEvent> {
        self.start_run_with_context(records, poll_initial_steering, cancellation, None)
    }

    fn start_run_with_context<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        poll_initial_steering: bool,
        cancellation: CancellationToken,
        initial_context_records: Option<Vec<AgentRecord>>,
    ) -> SendBoxStream<'a, AgentEvent> {
        self.require_idle()
            .expect("a borrowed Agent cannot safely start a second active run");
        let run_id = RunId::new(format!("agent-run-{}", self.next_sequence));
        let cancellation = cancellation.child();
        self.queue_rx
            .register_run(run_id.clone(), cancellation.clone())
            .expect("an open idle agent must accept its run registration");
        self.active_run = Some(run_id.clone());
        self.last_error = None;
        self.streaming = None;
        self.pending_tool_calls = Arc::from([]);
        let guard = SendRunGuard {
            agent: self,
            run_id,
            cancellation,
            finished: false,
        };
        Box::pin(crate::agent_run_stream!(
            guard,
            execute_send_tool_batch,
            records,
            poll_initial_steering,
            initial_context_records
        ))
    }

    fn make_prompt_text(&mut self, prompt: PromptText) -> AgentRecord {
        let message_id = self.allocate_message_id("user");
        let mut content = vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{}-text-0", message_id.as_str())),
            text: prompt.text,
        }];
        for (index, image) in prompt.images.into_iter().enumerate() {
            content.push(ContentBlock::Image {
                id: ContentBlockId::new(format!("{}-image-{index}", message_id.as_str())),
                data: image.data,
                mime_type: image.mime_type,
            });
        }
        AgentRecord::Llm(Message::User(UserMessage {
            id: message_id,
            content,
            // The executor-neutral core has no wall clock. Hosts that require
            // wall time supply an identified UserMessage through prompt_records.
            timestamp: Timestamp::default(),
        }))
    }

    fn allocate_message_id(&mut self, prefix: &str) -> MessageId {
        loop {
            let id = MessageId::new(format!("agent-{prefix}-{}", self.next_identity));
            self.next_identity = self.next_identity.saturating_add(1);
            if !self
                .state
                .transcript
                .iter()
                .any(|record| record.message_id() == Some(&id))
            {
                return id;
            }
        }
    }

    fn require_idle(&self) -> Result<(), AgentError> {
        if self.active_run.is_some() {
            Err(AgentError::RunActive)
        } else {
            Ok(())
        }
    }
}

impl LocalAgent {
    /// Creates an idle local/WASM agent around explicit local capabilities.
    pub fn new(
        runtime: Rc<dyn pi_ai::LocalModelRuntime>,
        state: AgentState,
        tools: LocalToolRegistry,
    ) -> Result<Self, AgentError> {
        tools.validate()?;
        let (control, queue_rx) = AgentControl::channel(crate::DEFAULT_QUEUE_CAPACITY);
        let defaults = LocalAgentDefaults::new(&state, &tools);
        Ok(Self {
            runtime,
            state,
            tools,
            next_sequence: crate::AGENT_INITIAL_SEQUENCE,
            streaming: None,
            pending_tool_calls: Arc::from([]),
            control,
            queue_rx,
            active_run: None,
            phase: None,
            last_error: None,
            options: SimpleGenerationOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            tool_scheduler: LocalToolScheduler::default(),
            context_policy: Rc::new(DefaultContextPolicy),
            message_projector: Rc::new(DefaultMessageProjector),
            turn_policy: Rc::new(DefaultTurnPolicy),
            defaults,
            next_identity: 1,
        })
    }

    /// Returns a cloneable queue/cancellation control handle.
    pub fn control(&self) -> AgentControl {
        self.control.clone()
    }

    /// Sets local steering drain behavior.
    pub fn set_steering_mode(&self, mode: crate::QueueDrainMode) {
        self.control.set_steering_mode(mode);
    }

    /// Returns local steering drain behavior.
    pub fn steering_mode(&self) -> crate::QueueDrainMode {
        self.control.steering_mode()
    }

    /// Sets local follow-up drain behavior.
    pub fn set_follow_up_mode(&self, mode: crate::QueueDrainMode) {
        self.control.set_follow_up_mode(mode);
    }

    /// Returns local follow-up drain behavior.
    pub fn follow_up_mode(&self) -> crate::QueueDrainMode {
        self.control.follow_up_mode()
    }

    /// Clears queued local steering records.
    pub fn clear_steering_queue(&self) -> usize {
        self.control.clear_steering()
    }

    /// Clears queued local follow-up records.
    pub fn clear_follow_up_queue(&self) -> usize {
        self.control.clear_follow_up()
    }

    /// Clears both local semantic queues.
    pub fn clear_all_queues(&self) -> usize {
        self.control.clear_all()
    }

    /// Returns the active run identity.
    pub fn active_run_id(&self) -> Option<&RunId> {
        self.active_run.as_ref()
    }

    /// Returns the current transient phase.
    pub fn phase(&self) -> Option<AgentPhase> {
        self.phase
    }

    /// Returns the last failed assistant's structured error.
    pub fn last_error(&self) -> Option<&PublicError> {
        self.last_error.as_ref()
    }

    /// Returns mutable local durable state while idle.
    pub fn state_mut(&mut self) -> Result<&mut AgentState, AgentError> {
        self.require_idle()?;
        Ok(&mut self.state)
    }

    /// Returns common local model options retained across transcript reset.
    pub fn options(&self) -> &SimpleGenerationOptions {
        &self.options
    }

    /// Mutates common local model options while idle.
    pub fn options_mut(&mut self) -> Result<&mut SimpleGenerationOptions, AgentError> {
        self.require_idle()?;
        Ok(&mut self.options)
    }

    /// Sets local batch scheduling behavior.
    pub fn set_tool_execution_mode(&mut self, mode: ToolExecutionMode) -> Result<(), AgentError> {
        self.require_idle()?;
        self.tool_execution = mode;
        Ok(())
    }

    /// Replaces local deterministic tool authorization and finalization policy.
    pub fn set_tool_policy(&mut self, policy: Rc<dyn LocalToolPolicy>) -> Result<(), AgentError> {
        self.require_idle()?;
        self.tool_scheduler = LocalToolScheduler::new(policy);
        Ok(())
    }

    /// Replaces the local context policy while idle.
    pub fn set_context_policy(
        &mut self,
        policy: Rc<dyn LocalContextPolicy>,
    ) -> Result<(), AgentError> {
        self.require_idle()?;
        self.context_policy = policy;
        Ok(())
    }

    /// Replaces the local Agent-record to canonical-message projector.
    pub fn set_message_projector(
        &mut self,
        projector: Rc<dyn LocalMessageProjector>,
    ) -> Result<(), AgentError> {
        self.require_idle()?;
        self.message_projector = projector;
        Ok(())
    }

    /// Replaces the local post-turn policy while idle.
    pub fn set_turn_policy(&mut self, policy: Rc<dyn LocalTurnPolicy>) -> Result<(), AgentError> {
        self.require_idle()?;
        self.turn_policy = policy;
        Ok(())
    }

    /// Starts one local prompt run.
    pub fn run<'a>(
        &'a mut self,
        input: AgentInput,
        cancellation: CancellationToken,
    ) -> LocalBoxStream<'a, AgentEvent> {
        self.start_run(input.records, true, cancellation)
    }

    /// Constructs and runs one local text/image prompt.
    pub fn prompt_text<'a>(
        &'a mut self,
        prompt: PromptText,
        cancellation: CancellationToken,
    ) -> LocalBoxStream<'a, AgentEvent> {
        let record = self.make_prompt_text(prompt);
        self.start_run(vec![record], true, cancellation)
    }

    /// Runs an already identified local prompt record batch.
    pub fn prompt_records<'a>(
        &'a mut self,
        records: impl IntoIterator<Item = AgentRecord>,
        cancellation: CancellationToken,
    ) -> LocalBoxStream<'a, AgentEvent> {
        self.start_run(records.into_iter().collect(), true, cancellation)
    }

    /// Continues a local run with Pi's assistant-tail queue drain behavior.
    pub fn continue_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<LocalBoxStream<'a, AgentEvent>, AgentError> {
        self.require_idle()?;
        let Some(tail) = self.state.transcript.last() else {
            return Err(AgentError::ContinueWithoutMessages);
        };
        if matches!(tail, AgentRecord::Llm(Message::Assistant(_))) {
            let (steering, follow_up) = self.queue_rx.drain_continue_tail();
            if !steering.is_empty() {
                return Ok(self.start_run(commands_to_records(steering), false, cancellation));
            }
            if !follow_up.is_empty() {
                return Ok(self.start_run(commands_to_records(follow_up), true, cancellation));
            }
            return Err(AgentError::ContinueFromAssistant);
        }
        Ok(self.start_run(Vec::new(), true, cancellation))
    }

    /// Retries the request boundary preceding a local failed assistant while
    /// retaining that failed record in durable state.
    pub fn retry_last_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<LocalBoxStream<'a, AgentEvent>, AgentError> {
        self.require_idle()?;
        if !tail_is_failed_assistant(&self.state.transcript) {
            return Err(AgentError::RetryRequiresFailedAssistant);
        }
        let retry_records = self.state.transcript[..self.state.transcript.len() - 1].to_vec();
        Ok(self.start_run_with_context(Vec::new(), true, cancellation, Some(retry_records)))
    }

    /// Clears local transcript and runtime scratch with Pi retention semantics.
    pub fn reset_transcript(&mut self) -> Result<(), AgentError> {
        self.require_idle()?;
        self.state.transcript.clear();
        self.streaming = None;
        self.pending_tool_calls = Arc::from([]);
        self.last_error = None;
        self.phase = None;
        self.queue_rx.clear_all();
        Ok(())
    }

    /// Restores local builder defaults and clears transcript/run scratch.
    pub fn reset_all(&mut self) -> Result<(), AgentError> {
        self.reset_transcript()?;
        self.state.system_prompt = self.defaults.system_prompt.clone();
        self.state.model = self.defaults.model.clone();
        self.state.reasoning = self.defaults.reasoning;
        self.tools = self.defaults.tools.clone();
        self.options = SimpleGenerationOptions::default();
        self.tool_execution = ToolExecutionMode::Parallel;
        self.tool_scheduler = LocalToolScheduler::default();
        self.control.set_steering_mode(crate::QueueDrainMode::One);
        self.control.set_follow_up_mode(crate::QueueDrainMode::One);
        self.context_policy = Rc::new(DefaultContextPolicy);
        self.message_projector = Rc::new(DefaultMessageProjector);
        self.turn_policy = Rc::new(DefaultTurnPolicy);
        Ok(())
    }

    fn start_run<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        poll_initial_steering: bool,
        cancellation: CancellationToken,
    ) -> LocalBoxStream<'a, AgentEvent> {
        self.start_run_with_context(records, poll_initial_steering, cancellation, None)
    }

    fn start_run_with_context<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        poll_initial_steering: bool,
        cancellation: CancellationToken,
        initial_context_records: Option<Vec<AgentRecord>>,
    ) -> LocalBoxStream<'a, AgentEvent> {
        self.require_idle()
            .expect("a borrowed LocalAgent cannot safely start a second active run");
        let run_id = RunId::new(format!("agent-run-{}", self.next_sequence));
        let cancellation = cancellation.child();
        self.queue_rx
            .register_run(run_id.clone(), cancellation.clone())
            .expect("an open idle local agent must accept its run registration");
        self.active_run = Some(run_id.clone());
        self.last_error = None;
        self.streaming = None;
        self.pending_tool_calls = Arc::from([]);
        let guard = LocalRunGuard {
            agent: self,
            run_id,
            cancellation,
            finished: false,
        };
        Box::pin(crate::agent_run_stream!(
            guard,
            execute_local_tool_batch,
            records,
            poll_initial_steering,
            initial_context_records
        ))
    }

    fn make_prompt_text(&mut self, prompt: PromptText) -> AgentRecord {
        let message_id = self.allocate_message_id("user");
        let mut content = vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{}-text-0", message_id.as_str())),
            text: prompt.text,
        }];
        for (index, image) in prompt.images.into_iter().enumerate() {
            content.push(ContentBlock::Image {
                id: ContentBlockId::new(format!("{}-image-{index}", message_id.as_str())),
                data: image.data,
                mime_type: image.mime_type,
            });
        }
        AgentRecord::Llm(Message::User(UserMessage {
            id: message_id,
            content,
            timestamp: Timestamp::default(),
        }))
    }

    fn allocate_message_id(&mut self, prefix: &str) -> MessageId {
        loop {
            let id = MessageId::new(format!("agent-{prefix}-{}", self.next_identity));
            self.next_identity = self.next_identity.saturating_add(1);
            if !self
                .state
                .transcript
                .iter()
                .any(|record| record.message_id() == Some(&id))
            {
                return id;
            }
        }
    }

    fn require_idle(&self) -> Result<(), AgentError> {
        if self.active_run.is_some() {
            Err(AgentError::RunActive)
        } else {
            Ok(())
        }
    }
}

struct SendRunGuard<'a> {
    agent: &'a mut crate::Agent,
    run_id: RunId,
    cancellation: CancellationToken,
    finished: bool,
}

impl Drop for SendRunGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.cancellation.cancel();
        }
        self.agent.queue_rx.unregister_run(&self.run_id);
        self.agent.active_run = None;
        self.agent.phase = None;
        self.agent.streaming = None;
        self.agent.pending_tool_calls = Arc::from([]);
    }
}

struct LocalRunGuard<'a> {
    agent: &'a mut LocalAgent,
    run_id: RunId,
    cancellation: CancellationToken,
    finished: bool,
}

impl Drop for LocalRunGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.cancellation.cancel();
        }
        self.agent.queue_rx.unregister_run(&self.run_id);
        self.agent.active_run = None;
        self.agent.phase = None;
        self.agent.streaming = None;
        self.agent.pending_tool_calls = Arc::from([]);
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! agent_run_stream {
    (
        $guard:ident,
        $execute_tools:ident,
        $records:expr,
        $poll_initial_steering:expr,
        $initial_context_records:expr
    ) => {
        async_stream::stream! {
            let mut $guard = $guard;
            let run_id = $guard.run_id.clone();
            let mut pending_records = $records;
            let mut current_context = $guard.agent.run_context();
            if let Some(initial_records) = $initial_context_records {
                current_context.records = initial_records;
            }
            // Pi's `newMessages` is cumulative for one loop invocation and is
            // not the same value as the replaceable current context. Prompt
            // records enter it; pre-existing continuation/retry history does
            // not.
            let mut new_messages = Vec::<AgentRecord>::new();
            let mut current_model = $guard.agent.state.model.clone();
            let mut current_reasoning = $guard.agent.state.reasoning;
            let mut turn = 0_u32;
            let mut run_usage: Option<Usage> = None;
            let mut run_cost: Option<pi_ai::Cost> = None;
            let mut run_cost_complete = true;

            $guard.agent.phase = Some(AgentPhase::StartRun);
            $guard.agent.bump_event_sequence();
            yield AgentEvent::RunStarted { run_id: run_id.clone() };

            loop {
                $guard.agent.bump_event_sequence();
                yield AgentEvent::TurnStarted {
                    run_id: run_id.clone(),
                    turn,
                    model: current_model.clone(),
                };

                $guard.agent.phase = Some(AgentPhase::InjectPendingMessages);
                for record in std::mem::take(&mut pending_records) {
                    let lifecycle_id = record
                        .message_id()
                        .cloned()
                        .unwrap_or_else(|| $guard.agent.allocate_message_id("custom"));
                    let role = record_role(&record);
                    $guard.agent.bump_event_sequence();
                    yield AgentEvent::MessageStarted {
                        message_id: lifecycle_id,
                        role,
                    };
                    $guard.agent.state.transcript.push(record.clone());
                    current_context.records.push(record.clone());
                    new_messages.push(record.clone());
                    $guard.agent.bump_event_sequence();
                    yield AgentEvent::MessageCommitted { message: record };
                }

                if turn == 0 {
                    $guard.agent.phase = Some(AgentPhase::InitialQueuePoll);
                    if $poll_initial_steering {
                        pending_records.extend(commands_to_records(
                            $guard.agent.queue_rx.drain(QueueKind::Steering),
                        ));
                    }

                    $guard.agent.phase = Some(AgentPhase::InjectPendingMessages);
                    for record in std::mem::take(&mut pending_records) {
                        let lifecycle_id = record
                            .message_id()
                            .cloned()
                            .unwrap_or_else(|| $guard.agent.allocate_message_id("custom"));
                        let role = record_role(&record);
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageStarted {
                            message_id: lifecycle_id,
                            role,
                        };
                        $guard.agent.state.transcript.push(record.clone());
                        current_context.records.push(record.clone());
                        new_messages.push(record.clone());
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageCommitted { message: record };
                    }
                }

                $guard.agent.phase = Some(AgentPhase::PrepareContext);
                let prepared = $guard.agent.prepare_context(
                    &current_context,
                    &current_model,
                    current_reasoning,
                    $guard.cancellation.clone(),
                ).await;

                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let cancelled = matches!(error, ContextError::Cancelled)
                            || $guard.cancellation.is_cancelled();
                        let message_id = $guard.agent.allocate_message_id("assistant");
                        let message = if cancelled {
                            empty_cancelled_message(
                                message_id,
                                &current_model,
                                CancellationReason::new(error.to_string()),
                            )
                        } else {
                            empty_failed_message(
                                message_id,
                                &current_model,
                                public_policy_error("context_policy", error.to_string()),
                            )
                        };
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageStarted {
                            message_id: message.id.clone(),
                            role: MessageRole::Assistant,
                        };
                        $guard.agent.state.transcript.push(AgentRecord::Llm(Message::Assistant(message.clone())));
                        current_context.records.push(AgentRecord::Llm(Message::Assistant(message.clone())));
                        new_messages.push(AgentRecord::Llm(Message::Assistant(message.clone())));
                        $guard.agent.last_error = message.finish.error.clone();
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageCommitted {
                            message: AgentRecord::Llm(Message::Assistant(message.clone())),
                        };
                        let outcome = turn_outcome(&message, Vec::new());
                        $guard.agent.phase = Some(AgentPhase::FinishTurn);
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::TurnFinished { outcome };
                        let run_outcome = terminal_run_outcome(&message);
                        $guard.agent.phase = Some(AgentPhase::FinishRun);
                        $guard.finished = true;
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::RunFinished { outcome: run_outcome };
                        return;
                    }
                };

                let PreparedContext {
                    context: prepared_context,
                    model_override,
                    options_override,
                    report,
                } = prepared;
                if let Some(model) = model_override {
                    current_model = model;
                }
                $guard.agent.bump_event_sequence();
                yield AgentEvent::ContextPrepared {
                    turn,
                    target: current_model.clone(),
                    report,
                };
                let mut default_options = $guard.agent.options.clone();
                default_options.reasoning = match current_reasoning {
                    pi_ai::ReasoningLevel::Off => None,
                    reasoning => Some(reasoning),
                };
                let request_options = options_override.unwrap_or(default_options);

                $guard.agent.phase = Some(AgentPhase::RequestAssistant);
                let stream_result = $guard.agent.runtime.stream(
                    ModelRequest {
                        model: current_model.clone(),
                        context: prepared_context,
                        options: request_options,
                    },
                    $guard.cancellation.clone(),
                ).await;

                let assistant = match stream_result {
                    Err(error) => {
                        let message_id = $guard.agent.allocate_message_id("assistant");
                        let message = if $guard.cancellation.is_cancelled() {
                            empty_cancelled_message(
                                message_id,
                                &current_model,
                                CancellationReason::new("Request was aborted"),
                            )
                        } else {
                            empty_failed_message(
                                message_id,
                                &current_model,
                                request_start_public_error(error),
                            )
                        };
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageStarted {
                            message_id: message.id.clone(),
                            role: MessageRole::Assistant,
                        };
                        message
                    }
                    Ok(mut response) => {
                        let mut assembler = AssistantAssembler::new();
                        let mut outer_started = false;
                        let mut terminal = None;
                        let mut protocol_error = None;

                        while let Some(event) = response.next().await {
                            let terminal_message = event.terminal_message().cloned();
                            if !outer_started
                                && let Some(message) = terminal_message.as_ref()
                                && let Err(error) = validate_terminal_only_event(&event)
                            {
                                let failure = terminal_protocol_failure(message.clone(), error);
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::MessageStarted {
                                    message_id: failure.id.clone(),
                                    role: MessageRole::Assistant,
                                };
                                terminal = Some(failure);
                                break;
                            }
                            if let AssistantEvent::MessageStarted { message_id, .. } = &event {
                                match assembler.apply(&event) {
                                    Ok(()) => {
                                        $guard.agent.streaming = Some(assembler.snapshot());
                                    }
                                    Err(error) => {
                                        protocol_error = Some(error.to_string());
                                        break;
                                    }
                                }
                                outer_started = true;
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::MessageStarted {
                                    message_id: message_id.clone(),
                                    role: MessageRole::Assistant,
                                };
                            } else if terminal_message.is_some() && outer_started {
                                match assembler.apply(&event) {
                                    Ok(()) => {
                                        $guard.agent.streaming = Some(assembler.snapshot());
                                    }
                                    Err(error) => {
                                        protocol_error = Some(error.to_string());
                                        break;
                                    }
                                }
                            } else if terminal_message.is_none() {
                                match assembler.apply(&event) {
                                    Ok(()) => {
                                        $guard.agent.streaming = Some(assembler.snapshot());
                                    }
                                    Err(error) => {
                                        protocol_error = Some(error.to_string());
                                        break;
                                    }
                                }
                            }

                            let event_message_id = assembler.snapshot().id;
                            if !outer_started {
                                if let Some(message) = terminal_message.as_ref() {
                                    outer_started = true;
                                    $guard.agent.bump_event_sequence();
                                    yield AgentEvent::MessageStarted {
                                        message_id: message.id.clone(),
                                        role: MessageRole::Assistant,
                                    };
                                } else {
                                    protocol_error = Some(
                                        "assistant content preceded MessageStarted".into(),
                                    );
                                    break;
                                }
                            }
                            let update_id = terminal_message
                                .as_ref()
                                .map(|message| message.id.clone())
                                .unwrap_or_else(|| event_message_id.clone());
                            if event_message_id.as_str().is_empty()
                                && let Some(message) = terminal_message.as_ref()
                            {
                                $guard.agent.streaming = Some(snapshot_from_message(message));
                            }
                            $guard.agent.bump_event_sequence();
                            yield AgentEvent::AssistantUpdate {
                                message_id: update_id,
                                event,
                            };
                            if let Some(message) = terminal_message {
                                terminal = Some(message);
                                break;
                            }
                        }

                        if let Some(message) = terminal {
                            message
                        } else {
                            let error = public_policy_error(
                                "assistant_stream_protocol",
                                protocol_error.unwrap_or_else(|| {
                                    "provider stream ended without a terminal event".into()
                                }),
                            );
                            if outer_started {
                                assembler.finish_failed(error, None)
                            } else {
                                let message = empty_failed_message(
                                    $guard.agent.allocate_message_id("assistant"),
                                    &current_model,
                                    error,
                                );
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::MessageStarted {
                                    message_id: message.id.clone(),
                                    role: MessageRole::Assistant,
                                };
                                message
                            }
                        }
                    }
                };

                $guard.agent.phase = Some(AgentPhase::CommitAssistant);
                $guard.agent.streaming = None;
                $guard.agent.state.transcript.push(AgentRecord::Llm(Message::Assistant(assistant.clone())));
                current_context.records.push(AgentRecord::Llm(Message::Assistant(assistant.clone())));
                new_messages.push(AgentRecord::Llm(Message::Assistant(assistant.clone())));
                run_usage = Some(match run_usage.take() {
                    None => assistant.usage.clone(),
                    Some(total) => add_usage(total, &assistant.usage),
                });
                if run_cost_complete {
                    match add_cost(run_cost.take(), assistant.cost.as_ref()) {
                        Ok(total) => run_cost = total,
                        Err(()) => run_cost_complete = false,
                    }
                }
                $guard.agent.last_error = assistant.finish.error.clone();
                $guard.agent.bump_event_sequence();
                yield AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(assistant.clone())),
                };

                if matches!(
                    assistant.finish.reason,
                    AssistantFinishReason::Error | AssistantFinishReason::Aborted
                ) {
                    let outcome = turn_outcome(&assistant, Vec::new());
                    $guard.agent.phase = Some(AgentPhase::FinishTurn);
                    $guard.agent.bump_event_sequence();
                    yield AgentEvent::TurnFinished { outcome };
                    let run_outcome = terminal_run_outcome(&assistant);
                    $guard.agent.phase = Some(AgentPhase::FinishRun);
                    $guard.finished = true;
                    $guard.agent.bump_event_sequence();
                    yield AgentEvent::RunFinished { outcome: run_outcome };
                    return;
                }

                let tool_calls = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { call, .. } => Some(call.clone()),
                        ContentBlock::Text { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. } => None,
                    })
                    .collect::<Vec<_>>();
                let mut tool_messages = Vec::new();
                let mut terminate_batch = false;

                if !tool_calls.is_empty() {
                    $guard.agent.phase = Some(AgentPhase::PrepareToolBatch);
                    $guard.agent.pending_tool_calls = Arc::from([]);
                    // Clone capabilities before opening the scheduler stream so
                    // live lifecycle polling does not immutably borrow Agent
                    // while event sequence and pending-call state are updated.
                    let tool_scheduler = $guard.agent.tool_scheduler.clone();
                    let tool_context = current_context.clone();
                    let tool_registry = tool_context.tools.clone();
                    let mut batch_events = $execute_tools(
                        &tool_scheduler,
                        &tool_registry,
                        ToolBatchRequest {
                            assistant: &assistant,
                            calls: &tool_calls,
                            context: &tool_context,
                            configured_mode: $guard.agent.tool_execution,
                            cancellation: $guard.cancellation.clone(),
                        },
                    );
                    let mut batch_plan = None;
                    let batch = loop {
                        let Some(event) = batch_events.next().await else {
                            panic!("tool scheduler ended without a joined batch outcome");
                        };
                        match event {
                            ToolBatchStreamEvent::BatchStarted { plan } => {
                                batch_plan = Some(plan);
                            }
                            ToolBatchStreamEvent::CallStarted { call, .. } => {
                                $guard.agent.phase = Some(AgentPhase::PrepareToolBatch);
                                add_pending_call(
                                    &mut $guard.agent.pending_tool_calls,
                                    call.id.clone(),
                                );
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::ToolExecutionStarted { call };
                            }
                            ToolBatchStreamEvent::CallUpdated { call_id, update, .. } => {
                                $guard.agent.phase = Some(AgentPhase::ExecuteToolBatch);
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::ToolExecutionUpdated { call_id, update };
                            }
                            ToolBatchStreamEvent::CallFinished { outcome } => {
                                let outcome = *outcome;
                                let commit_immediately = matches!(
                                    batch_plan,
                                    Some($crate::ToolExecutionPlan::SequentialBatch)
                                );
                                let message = commit_immediately
                                    .then(|| tool_result_message(&assistant, &outcome));
                                $guard.agent.phase = Some(AgentPhase::ExecuteToolBatch);
                                remove_pending_call(
                                    &mut $guard.agent.pending_tool_calls,
                                    &outcome.call.id,
                                );
                                $guard.agent.bump_event_sequence();
                                yield AgentEvent::ToolExecutionFinished {
                                    call_id: outcome.call.id.clone(),
                                    result: outcome.output.clone(),
                                    is_error: outcome.is_error,
                                };
                                if let Some(message) = message {
                                    // Pinned Pi commits sequential and
                                    // truncation-synthesis results before the
                                    // next call lifecycle begins. Parallel
                                    // results remain deferred until the joined
                                    // batch completes.
                                    $guard.agent.phase = Some(AgentPhase::CommitToolResults);
                                    $guard.agent.bump_event_sequence();
                                    yield AgentEvent::MessageStarted {
                                        message_id: message.id.clone(),
                                        role: MessageRole::ToolResult,
                                    };
                                    $guard.agent.state.transcript.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                                    current_context.records.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                                    new_messages.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                                    $guard.agent.bump_event_sequence();
                                    yield AgentEvent::MessageCommitted {
                                        message: AgentRecord::Llm(Message::ToolResult(message.clone())),
                                    };
                                    tool_messages.push(message);
                                }
                            }
                            ToolBatchStreamEvent::BatchFinished { outcome } => break *outcome,
                        }
                    };
                    terminate_batch = batch.terminate;
                    // Every launched future has settled and cancellation may
                    // have skipped later sequential calls. None remain pending.
                    $guard.agent.pending_tool_calls = Arc::from([]);

                    if batch.plan == $crate::ToolExecutionPlan::ParallelBatch {
                        $guard.agent.phase = Some(AgentPhase::CommitToolResults);
                        for completed in batch.source_order {
                            let message = tool_result_message(&assistant, &completed);
                            $guard.agent.bump_event_sequence();
                            yield AgentEvent::MessageStarted {
                                message_id: message.id.clone(),
                                role: MessageRole::ToolResult,
                            };
                            $guard.agent.state.transcript.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                            current_context.records.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                            new_messages.push(AgentRecord::Llm(Message::ToolResult(message.clone())));
                            $guard.agent.bump_event_sequence();
                            yield AgentEvent::MessageCommitted {
                                message: AgentRecord::Llm(Message::ToolResult(message.clone())),
                            };
                            tool_messages.push(message);
                        }
                    } else {
                        debug_assert_eq!(tool_messages.len(), batch.source_order.len());
                    }
                }

                let outcome = turn_outcome(
                    &assistant,
                    tool_messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect(),
                );
                $guard.agent.phase = Some(AgentPhase::FinishTurn);
                $guard.agent.bump_event_sequence();
                yield AgentEvent::TurnFinished {
                    outcome: outcome.clone(),
                };

                $guard.agent.phase = Some(AgentPhase::PrepareNextTurn);
                let next = $guard.agent.turn_policy.prepare_next_turn(
                    CompletedTurn {
                        outcome: &outcome,
                        assistant: &assistant,
                        tool_results: &tool_messages,
                        context: &current_context,
                        new_messages: &new_messages,
                    },
                    $guard.cancellation.clone(),
                ).await;
                let next = match next {
                    Ok(next) => next,
                    Err(error) => {
                        let message_id = $guard.agent.allocate_message_id("assistant");
                        let failed = if $guard.cancellation.is_cancelled() {
                            empty_cancelled_message(
                                message_id,
                                &current_model,
                                CancellationReason::new(error.to_string()),
                            )
                        } else {
                            empty_failed_message(
                                message_id,
                                &current_model,
                                public_policy_error("turn_policy", error.to_string()),
                            )
                        };
                        $guard.agent.state.transcript.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        current_context.records.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        new_messages.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        $guard.agent.last_error = failed.finish.error.clone();
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageStarted {
                            message_id: failed.id.clone(),
                            role: MessageRole::Assistant,
                        };
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageCommitted {
                            message: AgentRecord::Llm(Message::Assistant(failed.clone())),
                        };
                        let failed_outcome = turn_outcome(&failed, Vec::new());
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::TurnFinished { outcome: failed_outcome };
                        $guard.agent.phase = Some(AgentPhase::FinishRun);
                        $guard.finished = true;
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::RunFinished {
                            outcome: terminal_run_outcome(&failed),
                        };
                        return;
                    }
                };
                apply_next_turn(
                    next,
                    &mut current_context,
                    &mut current_model,
                    &mut current_reasoning,
                );

                $guard.agent.phase = Some(AgentPhase::ShouldStopAfterTurn);
                let should_stop = $guard.agent.turn_policy.should_stop(
                    CompletedTurn {
                        outcome: &outcome,
                        assistant: &assistant,
                        tool_results: &tool_messages,
                        context: &current_context,
                        new_messages: &new_messages,
                    },
                    $guard.cancellation.clone(),
                ).await;
                let should_stop = match should_stop {
                    Ok(should_stop) => should_stop,
                    Err(error) => {
                        let message_id = $guard.agent.allocate_message_id("assistant");
                        let failed = if $guard.cancellation.is_cancelled() {
                            empty_cancelled_message(
                                message_id,
                                &current_model,
                                CancellationReason::new(error.to_string()),
                            )
                        } else {
                            empty_failed_message(
                                message_id,
                                &current_model,
                                public_policy_error("turn_policy", error.to_string()),
                            )
                        };
                        $guard.agent.state.transcript.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        current_context.records.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        new_messages.push(AgentRecord::Llm(Message::Assistant(failed.clone())));
                        $guard.agent.last_error = failed.finish.error.clone();
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageStarted {
                            message_id: failed.id.clone(),
                            role: MessageRole::Assistant,
                        };
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::MessageCommitted {
                            message: AgentRecord::Llm(Message::Assistant(failed.clone())),
                        };
                        let failed_outcome = turn_outcome(&failed, Vec::new());
                        $guard.agent.phase = Some(AgentPhase::FinishTurn);
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::TurnFinished { outcome: failed_outcome };
                        $guard.agent.phase = Some(AgentPhase::FinishRun);
                        $guard.finished = true;
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::RunFinished {
                            outcome: terminal_run_outcome(&failed),
                        };
                        return;
                    }
                };

                let cancelled_during_tools = $guard.cancellation.is_cancelled()
                    && !tool_calls.is_empty();
                if should_stop && !cancelled_during_tools {
                    let final_id = assistant.id.clone();
                    $guard.agent.phase = Some(AgentPhase::FinishRun);
                    $guard.finished = true;
                    $guard.agent.bump_event_sequence();
                    yield AgentEvent::RunFinished {
                        outcome: RunOutcome::Completed {
                            final_message_id: final_id,
                            usage: run_usage.clone().unwrap_or_else(|| Usage::zero(UsageSource::Unknown)),
                            cost: run_cost_complete.then(|| run_cost.clone()).flatten(),
                        },
                    };
                    return;
                }

                $guard.agent.phase = Some(AgentPhase::PollSteering);
                pending_records = commands_to_records(
                    $guard.agent.queue_rx.drain(QueueKind::Steering),
                );
                if pending_records.is_empty()
                    && !cancelled_during_tools
                    && (tool_calls.is_empty() || terminate_batch)
                {
                    $guard.agent.phase = Some(AgentPhase::WouldStop);
                    $guard.agent.phase = Some(AgentPhase::PollFollowUp);
                    pending_records = commands_to_records(
                        $guard.agent.queue_rx.drain(QueueKind::FollowUp),
                    );
                    if pending_records.is_empty() {
                        let final_id = assistant.id.clone();
                        $guard.agent.phase = Some(AgentPhase::FinishRun);
                        $guard.finished = true;
                        $guard.agent.bump_event_sequence();
                        yield AgentEvent::RunFinished {
                            outcome: RunOutcome::Completed {
                                final_message_id: final_id,
                                usage: run_usage.clone().unwrap_or_else(|| Usage::zero(UsageSource::Unknown)),
                                cost: run_cost_complete.then(|| run_cost.clone()).flatten(),
                            },
                        };
                        return;
                    }
                }

                turn = turn.saturating_add(1);
            }
        }
    };
}

impl crate::Agent {
    fn run_context(&self) -> AgentContext {
        AgentRunContext {
            system_prompt: self.state.system_prompt.clone(),
            records: self.state.transcript.clone(),
            tools: self.tools.clone(),
        }
    }

    async fn prepare_context(
        &self,
        context: &AgentContext,
        model: &pi_ai::ModelRef,
        reasoning: pi_ai::ReasoningLevel,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext, ContextError> {
        let tool_specs = context
            .tools
            .iter()
            .map(|(_, tool)| tool.spec().clone())
            .collect::<Vec<_>>();
        let prepared = self
            .context_policy
            .prepare_agent_records(
                AgentStateView {
                    state: &self.state,
                    records: &context.records,
                    tools: &tool_specs,
                    model,
                    reasoning,
                },
                cancellation,
            )
            .await?;
        let messages = self.message_projector.project(&prepared.records).await?;
        let context = Context {
            schema_version: pi_ai::CONTEXT_SCHEMA_VERSION,
            system_prompt: (!context.system_prompt.is_empty())
                .then(|| context.system_prompt.clone()),
            messages,
            tools: tool_specs,
        };
        let report_target = prepared.model_override.as_ref().unwrap_or(model);
        let report = prepared
            .report
            .unwrap_or_else(|| provider_neutral_handoff_report(report_target, &context.messages));
        Ok(PreparedContext {
            context,
            model_override: prepared.model_override,
            options_override: prepared.options_override,
            report,
        })
    }

    fn bump_event_sequence(&mut self) {
        self.next_sequence = self.next_sequence.saturating_add(1);
    }
}

impl LocalAgent {
    fn run_context(&self) -> LocalAgentContext {
        AgentRunContext {
            system_prompt: self.state.system_prompt.clone(),
            records: self.state.transcript.clone(),
            tools: self.tools.clone(),
        }
    }

    async fn prepare_context(
        &self,
        context: &LocalAgentContext,
        model: &pi_ai::ModelRef,
        reasoning: pi_ai::ReasoningLevel,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext, ContextError> {
        let tool_specs = context
            .tools
            .iter()
            .map(|(_, tool)| tool.spec().clone())
            .collect::<Vec<_>>();
        let prepared = self
            .context_policy
            .prepare_agent_records(
                AgentStateView {
                    state: &self.state,
                    records: &context.records,
                    tools: &tool_specs,
                    model,
                    reasoning,
                },
                cancellation,
            )
            .await?;
        let messages = self.message_projector.project(&prepared.records).await?;
        let context = Context {
            schema_version: pi_ai::CONTEXT_SCHEMA_VERSION,
            system_prompt: (!context.system_prompt.is_empty())
                .then(|| context.system_prompt.clone()),
            messages,
            tools: tool_specs,
        };
        let report_target = prepared.model_override.as_ref().unwrap_or(model);
        let report = prepared
            .report
            .unwrap_or_else(|| provider_neutral_handoff_report(report_target, &context.messages));
        Ok(PreparedContext {
            context,
            model_override: prepared.model_override,
            options_override: prepared.options_override,
            report,
        })
    }

    fn bump_event_sequence(&mut self) {
        self.next_sequence = self.next_sequence.saturating_add(1);
    }
}

fn provider_neutral_handoff_report(
    model: &pi_ai::ModelRef,
    messages: &[pi_ai::Message],
) -> pi_ai::HandoffReport {
    let mut report = pi_ai::HandoffReport::unchanged(pi_ai::ModelFingerprint::new(
        model.provider.clone(),
        pi_ai::ApiId::new("provider-neutral"),
        model.model.clone(),
    ));
    for message in messages {
        if let pi_ai::Message::Assistant(assistant) = message {
            report.source_models.insert(pi_ai::ModelFingerprint::new(
                assistant.provider.clone(),
                assistant.api.clone(),
                assistant
                    .response_model
                    .clone()
                    .unwrap_or_else(|| assistant.requested_model.clone()),
            ));
        }
    }
    report
}

impl Drop for crate::Agent {
    fn drop(&mut self) {
        self.queue_rx.close();
    }
}

impl Drop for LocalAgent {
    fn drop(&mut self) {
        self.queue_rx.close();
    }
}

fn execute_send_tool_batch<'a>(
    scheduler: &'a ToolScheduler,
    tools: &'a ToolRegistry,
    request: ToolBatchRequest<'a, ToolRegistry>,
) -> SendBoxStream<'a, ToolBatchStreamEvent> {
    scheduler.execute_batch_events(tools, request)
}

fn execute_local_tool_batch<'a>(
    scheduler: &'a LocalToolScheduler,
    tools: &'a LocalToolRegistry,
    request: ToolBatchRequest<'a, LocalToolRegistry>,
) -> LocalBoxStream<'a, ToolBatchStreamEvent> {
    scheduler.execute_batch_events(tools, request)
}

fn tool_result_message(
    assistant: &AssistantMessage,
    completed: &ToolCallOutcome,
) -> ToolResultMessage {
    ToolResultMessage {
        id: MessageId::new(format!(
            "{}-tool-result-{}",
            assistant.id.as_str(),
            completed.source_index.0
        )),
        tool_call_id: completed.call.id.clone(),
        tool_name: completed.call.name.clone(),
        content: completed.output.content.clone(),
        details: completed
            .output
            .details
            .clone()
            .map(|value| VersionedExtension {
                schema_version: 1,
                value,
            }),
        usage: completed.output.usage.clone(),
        added_tool_names: completed.output.added_tool_names.clone(),
        is_error: completed.is_error,
        timestamp: assistant.timestamp,
    }
}

fn remove_pending_call(pending: &mut Arc<[ToolCallId]>, completed: &ToolCallId) {
    *pending = pending
        .iter()
        .filter(|call_id| *call_id != completed)
        .cloned()
        .collect::<Vec<_>>()
        .into();
}

fn add_pending_call(pending: &mut Arc<[ToolCallId]>, call_id: ToolCallId) {
    let mut calls = pending.to_vec();
    calls.push(call_id);
    *pending = calls.into();
}

fn commands_to_records(commands: Vec<crate::QueueCommand>) -> Vec<AgentRecord> {
    commands
        .into_iter()
        .map(|command| command.message)
        .collect()
}

fn record_role(record: &AgentRecord) -> MessageRole {
    match record {
        AgentRecord::Llm(Message::User(_)) => MessageRole::User,
        AgentRecord::Llm(Message::Assistant(_)) => MessageRole::Assistant,
        AgentRecord::Llm(Message::ToolResult(_)) => MessageRole::ToolResult,
        AgentRecord::Custom { .. } => MessageRole::Custom,
    }
}

fn tail_is_failed_assistant(records: &[AgentRecord]) -> bool {
    matches!(
        records.last(),
        Some(AgentRecord::Llm(Message::Assistant(message)))
            if matches!(
                message.finish.reason,
                AssistantFinishReason::Error | AssistantFinishReason::Aborted
            )
    )
}

fn request_start_public_error(error: RequestStartError) -> PublicError {
    let code = match error.kind {
        RequestStartErrorKind::InvalidRequest => "invalid_request",
        RequestStartErrorKind::UnknownProvider => "unknown_provider",
        RequestStartErrorKind::UnknownModel => "unknown_model",
        RequestStartErrorKind::RuntimeUnavailable => "runtime_unavailable",
        RequestStartErrorKind::Internal => "internal",
        _ => "request_start_error",
    };
    PublicError {
        code: code.into(),
        message: error.message,
        retryable: error.kind == RequestStartErrorKind::RuntimeUnavailable,
        provider_code: None,
        status: None,
        request_id: None,
    }
}

fn public_policy_error(code: &str, message: String) -> PublicError {
    PublicError {
        code: code.into(),
        message,
        retryable: false,
        provider_code: None,
        status: None,
        request_id: None,
    }
}

fn validate_terminal_only_event(event: &AssistantEvent) -> Result<(), String> {
    let (message, terminal_kind) = match event {
        AssistantEvent::Finished { message } => (message, "finished"),
        AssistantEvent::Failed { message } => (message, "failed"),
        AssistantEvent::Cancelled { message } => (message, "cancelled"),
        _ => return Err("standalone assistant event was not terminal".into()),
    };

    match event {
        AssistantEvent::Finished { .. } => {
            if !matches!(
                message.finish.reason,
                AssistantFinishReason::Stop
                    | AssistantFinishReason::Length
                    | AssistantFinishReason::ToolUse
                    | AssistantFinishReason::Deferred
            ) || message.finish.error.is_some()
            {
                return Err(format!(
                    "terminal-only {terminal_kind} event carried invalid successful finish metadata"
                ));
            }
            if let Some(item) = message
                .replay
                .items
                .iter()
                .find(|item| item.completeness == ReplayCompleteness::Incomplete)
            {
                return Err(format!(
                    "terminal-only successful message contains incomplete replay item {}",
                    item.id
                ));
            }
        }
        AssistantEvent::Failed { .. } => {
            if message.finish.reason != AssistantFinishReason::Error {
                return Err(format!(
                    "terminal-only {terminal_kind} event carried finish reason {:?}",
                    message.finish.reason
                ));
            }
            if message.finish.error.is_none() {
                return Err("terminal-only failed event omitted its public error".into());
            }
        }
        AssistantEvent::Cancelled { .. } => {
            if message.finish.reason != AssistantFinishReason::Aborted {
                return Err(format!(
                    "terminal-only {terminal_kind} event carried finish reason {:?}",
                    message.finish.reason
                ));
            }
            let Some(error) = message.finish.error.as_ref() else {
                return Err("terminal-only cancelled event omitted its public error".into());
            };
            if message.finish.raw_provider_reason.is_some()
                || error.code != "cancelled"
                || error.retryable
                || error.provider_code.is_some()
                || error.status.is_some()
            {
                return Err(
                    "terminal-only cancelled event carried invalid cancellation metadata".into(),
                );
            }
        }
        _ => unreachable!("terminal event was classified above"),
    }
    Ok(())
}

fn terminal_protocol_failure(mut message: AssistantMessage, error: String) -> AssistantMessage {
    message.finish = AssistantFinish {
        reason: AssistantFinishReason::Error,
        raw_provider_reason: None,
        error: Some(public_policy_error("assistant_stream_protocol", error)),
    };
    message
}

fn empty_failed_message(
    id: MessageId,
    model: &pi_ai::ModelRef,
    error: PublicError,
) -> AssistantMessage {
    empty_terminal_message(id, model, AssistantFinishReason::Error, error)
}

fn empty_cancelled_message(
    id: MessageId,
    model: &pi_ai::ModelRef,
    reason: CancellationReason,
) -> AssistantMessage {
    empty_terminal_message(
        id,
        model,
        AssistantFinishReason::Aborted,
        PublicError {
            code: "cancelled".into(),
            message: reason.message,
            retryable: false,
            provider_code: None,
            status: None,
            request_id: reason.request_id,
        },
    )
}

fn empty_terminal_message(
    id: MessageId,
    model: &pi_ai::ModelRef,
    reason: AssistantFinishReason,
    error: PublicError,
) -> AssistantMessage {
    let api = ApiId::new("unknown");
    AssistantMessage {
        id,
        provider: model.provider.clone(),
        api: api.clone(),
        requested_model: model.model.clone(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: Vec::new(),
        replay: ReplayEnvelope::new(ReplayScope::new(
            model.provider.clone(),
            api,
            model.model.clone(),
            model.model.clone(),
        )),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error: Some(error),
        },
        timestamp: Timestamp::default(),
    }
}

fn snapshot_from_message(message: &AssistantMessage) -> AssistantMessageSnapshot {
    AssistantMessageSnapshot {
        id: message.id.clone(),
        provider: message.provider.clone(),
        api: message.api.clone(),
        requested_model: message.requested_model.clone(),
        response_model: message.response_model.clone(),
        response_id: message.response_id.clone(),
        end_turn: message.end_turn,
        diagnostics: message.diagnostics.clone(),
        content: message.content.clone(),
        replay: message.replay.clone(),
        usage: message.usage.clone(),
        cost: message.cost.clone(),
        timestamp: message.timestamp,
        terminal_message: Some(message.clone()),
    }
}

fn turn_outcome(
    assistant: &AssistantMessage,
    tool_result_message_ids: Vec<MessageId>,
) -> TurnOutcome {
    TurnOutcome {
        assistant_message_id: assistant.id.clone(),
        assistant_finish: assistant.finish.reason,
        tool_result_message_ids,
        usage: assistant.usage.clone(),
        cost: assistant.cost.clone(),
    }
}

fn terminal_run_outcome(message: &AssistantMessage) -> RunOutcome {
    match message.finish.reason {
        AssistantFinishReason::Error => RunOutcome::Failed {
            committed_message_id: message.id.clone(),
            error: message.finish.error.clone().unwrap_or_else(|| {
                public_policy_error("missing_error", "failed assistant omitted its error".into())
            }),
        },
        AssistantFinishReason::Aborted => {
            let error = message.finish.error.as_ref();
            RunOutcome::Cancelled {
                committed_message_id: message.id.clone(),
                reason: CancellationReason {
                    message: error
                        .map(|error| error.message.clone())
                        .unwrap_or_else(|| "Request was aborted".into()),
                    request_id: error.and_then(|error| error.request_id.clone()),
                },
            }
        }
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => RunOutcome::Completed {
            final_message_id: message.id.clone(),
            usage: message.usage.clone(),
            cost: message.cost.clone(),
        },
    }
}

fn add_cost(
    total: Option<pi_ai::Cost>,
    next: Option<&pi_ai::Cost>,
) -> Result<Option<pi_ai::Cost>, ()> {
    let Some(next) = next else {
        return Err(());
    };
    let Some(mut total) = total else {
        return Ok(Some(next.clone()));
    };
    if total.currency != next.currency {
        return Err(());
    }
    total.micros = total.micros.checked_add(next.micros).ok_or(())?;
    Ok(Some(total))
}

fn add_usage(mut total: Usage, next: &Usage) -> Usage {
    total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
    total.reasoning_tokens = add_optional(total.reasoning_tokens, next.reasoning_tokens);
    total.cache_read_tokens = add_optional(total.cache_read_tokens, next.cache_read_tokens);
    total.cache_write_tokens = add_optional(total.cache_write_tokens, next.cache_write_tokens);
    total.cache_write_one_hour_tokens = add_optional(
        total.cache_write_one_hour_tokens,
        next.cache_write_one_hour_tokens,
    );
    if total.source != next.source {
        total.source = UsageSource::Mixed;
    }
    total
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

fn apply_next_turn<Tools>(
    next: NextTurn<Tools>,
    context: &mut AgentRunContext<Tools>,
    model: &mut pi_ai::ModelRef,
    reasoning: &mut pi_ai::ReasoningLevel,
) {
    if let Some(replacement) = next.context {
        *context = replacement;
    }
    if let Some(replacement) = next.model {
        *model = replacement;
    }
    if let Some(replacement) = next.reasoning {
        *reasoning = replacement;
    }
}
