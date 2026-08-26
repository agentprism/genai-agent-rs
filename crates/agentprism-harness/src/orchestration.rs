//! Durable orchestration over `pi-agent-core` and `pi-agent-session`.

use crate::{
    HarnessError, HarnessEvent, HarnessEventBus, HarnessRunOutcome, LocalHarnessEventBus,
    LocalSession, RecoveryCorruptionReason, Session, next_step_attempt, reconstruct_branch_context,
};
use agentprism_ai::{
    AssistantFinishReason, AssistantMessage, CancellationReason, CancellationToken, ContentBlock,
    ContentBlockId, Cost, LocalBoxFuture, LocalBoxStream, Message, MessageId, PublicError, RunId,
    SendBoxFuture, SendBoxStream, Timestamp, ToolCall, ToolCallId, ToolResultContent,
    ToolResultMessage, Usage, UsageSource, UserMessage, VersionedExtension,
};
use agentprism_core::{
    AfterToolCall, Agent, AgentContext, AgentControl, AgentError, AgentEvent, AgentRecord,
    BeforeToolCall, CompletedTurnRecoveryEvent, CompletedTurnRecoveryStream, LocalAfterToolCall,
    LocalAgent, LocalAgentContext, LocalBeforeToolCall, LocalCompletedTurnRecoveryStream,
    LocalToolPolicy, MessageRole, PromptText, QueueReceipt as AgentQueueReceipt,
    RecoveredCompletedTurn, RecoveredRunState, RunOutcome as AgentRunOutcome, ToolAuthorization,
    ToolBatchRequest, ToolBatchStreamEvent, ToolExecutionMode, ToolOutput, ToolOutputPatch,
    ToolPolicy,
};
use agentprism_session::{
    AppendReceipt, CompactionReason, EntryId, OperationIntent, OperationOutcome, OperationRecord,
    OperationStep, ProvisionedEntry, QueueKind, RecoveryDecision, SessionEntry, SessionState,
    ToolCallIdentity, ToolReplayPolicy,
};
use futures_util::{StreamExt, lock::Mutex as AsyncMutex};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
};

/// One observation from a durably driven core-agent run.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessRunEvent {
    /// Top-level lifecycle event matching pinned Pi's harness event contract.
    Harness(HarnessEvent),
    /// Lossless nested `pi-agent-core` lifecycle event.
    Agent(Box<AgentEvent>),
}

/// Borrowed, backpressured Send harness run stream.
pub type HarnessRunStream<'a> = SendBoxStream<'a, Result<HarnessRunEvent, HarnessError>>;

/// Borrowed, backpressured Local/WASM harness run stream.
pub type LocalHarnessRunStream<'a> = LocalBoxStream<'a, Result<HarnessRunEvent, HarnessError>>;

/// Durable acknowledgement for one queue ingress.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DurableQueueReceipt {
    /// Preallocated durable entry identity.
    pub entry_id: EntryId,
    /// Durable queue kind.
    pub queue: QueueKind,
    /// Session append receipt proving durable acceptance.
    pub durable: AppendReceipt,
    /// Bare-agent queue acknowledgement for active-run queues.
    pub agent: Option<AgentQueueReceipt>,
}

/// Lossless durable run intent supplied to a before-resume hook.
#[derive(Clone, Debug, PartialEq)]
pub struct RunResumeIntent {
    /// Stable operation identity.
    pub run_id: RunId,
    /// Normalized caller input captured before the original before-run hook.
    pub original_prompt: Vec<AgentRecord>,
    /// Provisioned initial entries captured after original run preparation.
    pub initial_messages: Vec<ProvisionedEntry>,
    /// Run-local system prompt override.
    pub system_prompt_override: Option<String>,
    /// Versioned extension state owned by resume hooks.
    pub resume_data: BTreeMap<String, VersionedExtension>,
}

/// Thread-safe before-resume hook for persisted run extension state.
pub trait RunResumeHook: Send + Sync + 'static {
    /// Reapplies one recorded run intent before recovery starts executing.
    fn before_resume<'a>(
        &'a self,
        intent: &'a RunResumeIntent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), HarnessError>>;
}

/// Local-executor counterpart of [`RunResumeHook`].
pub trait LocalRunResumeHook: 'static {
    /// Reapplies one recorded run intent before local recovery starts.
    fn before_resume<'a>(
        &'a self,
        intent: &'a RunResumeIntent,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<(), HarnessError>>;
}

#[derive(Clone, Copy, Debug, Default)]
struct NoopRunResumeHook;

impl RunResumeHook for NoopRunResumeHook {
    fn before_resume<'a>(
        &'a self,
        _intent: &'a RunResumeIntent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|reason| HarnessError::Context {
                    message: reason.to_string(),
                })
        })
    }
}

impl LocalRunResumeHook for NoopRunResumeHook {
    fn before_resume<'a>(
        &'a self,
        _intent: &'a RunResumeIntent,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|reason| HarnessError::Context {
                    message: reason.to_string(),
                })
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveHarnessRun {
    operation_run_id: RunId,
    agent_run_id: RunId,
    accepting_ingress: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurableToolStartState {
    Pending,
    Persisted,
}

#[derive(Clone, Debug)]
struct DurableToolStartIntent {
    tool_index: u32,
    call: ToolCallIdentity,
    result_entry_id: EntryId,
    replay: ToolReplayPolicy,
    state: DurableToolStartState,
}

#[derive(Clone, Debug)]
struct DurableToolPlan {
    run_id: RunId,
    assistant_message_id: MessageId,
    assistant_entry_id: EntryId,
    calls: BTreeMap<ToolCallId, DurableToolStartIntent>,
}

fn live_durable_tool_plan(
    session: &Session,
    run_id: RunId,
    assistant_entry_id: EntryId,
    assistant: &AssistantMessage,
    replay_policies: &BTreeMap<String, ToolReplayPolicy>,
) -> Result<DurableToolPlan, HarnessError> {
    durable_tool_plan(
        run_id,
        assistant_entry_id,
        assistant,
        replay_policies,
        |call| {
            (
                session.next_entry_id("tool-result"),
                DurableToolStartState::Pending,
                configured_tool_replay_policy(replay_policies, &call.name),
            )
        },
    )
}

fn local_live_durable_tool_plan(
    session: &LocalSession,
    run_id: RunId,
    assistant_entry_id: EntryId,
    assistant: &AssistantMessage,
    replay_policies: &BTreeMap<String, ToolReplayPolicy>,
) -> Result<DurableToolPlan, HarnessError> {
    durable_tool_plan(
        run_id,
        assistant_entry_id,
        assistant,
        replay_policies,
        |call| {
            (
                session.next_entry_id("tool-result"),
                DurableToolStartState::Pending,
                configured_tool_replay_policy(replay_policies, &call.name),
            )
        },
    )
}

fn durable_tool_plan(
    run_id: RunId,
    assistant_entry_id: EntryId,
    assistant: &AssistantMessage,
    replay_policies: &BTreeMap<String, ToolReplayPolicy>,
    mut disposition: impl FnMut(&ToolCall) -> (EntryId, DurableToolStartState, ToolReplayPolicy),
) -> Result<DurableToolPlan, HarnessError> {
    let mut calls = BTreeMap::new();
    for (index, call) in assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call),
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. } => None,
        })
        .enumerate()
    {
        let tool_index = u32::try_from(index).map_err(|_| HarnessError::IncompleteAgentStream)?;
        let (result_entry_id, state, replay) = disposition(call);
        calls.insert(
            call.id.clone(),
            DurableToolStartIntent {
                tool_index,
                call: ToolCallIdentity {
                    id: call.id.clone(),
                    name: call.name.clone(),
                },
                result_entry_id,
                replay,
                state,
            },
        );
    }
    let _ = replay_policies;
    Ok(DurableToolPlan {
        run_id,
        assistant_message_id: assistant.id.clone(),
        assistant_entry_id,
        calls,
    })
}

#[derive(Default)]
struct SendDurableToolState {
    plan: Mutex<Option<DurableToolPlan>>,
    error: Mutex<Option<HarnessError>>,
}

impl SendDurableToolState {
    fn install(&self, plan: DurableToolPlan) {
        *lock_unpoisoned(&self.plan) = Some(plan);
        *lock_unpoisoned(&self.error) = None;
    }

    fn intent(
        &self,
        assistant_message_id: &MessageId,
        call: &ToolCall,
    ) -> Result<(RunId, EntryId, DurableToolStartIntent), HarnessError> {
        let plan = lock_unpoisoned(&self.plan);
        let plan = plan
            .as_ref()
            .filter(|plan| &plan.assistant_message_id == assistant_message_id)
            .ok_or(HarnessError::IncompleteAgentStream)?;
        let intent = plan
            .calls
            .get(&call.id)
            .filter(|intent| intent.call.name == call.name)
            .cloned()
            .ok_or(HarnessError::IncompleteAgentStream)?;
        Ok((plan.run_id.clone(), plan.assistant_entry_id.clone(), intent))
    }

    fn mark_persisted(&self, assistant_message_id: &MessageId, call_id: &ToolCallId) {
        if let Some(plan) = lock_unpoisoned(&self.plan).as_mut()
            && &plan.assistant_message_id == assistant_message_id
            && let Some(intent) = plan.calls.get_mut(call_id)
        {
            intent.state = DurableToolStartState::Persisted;
        }
    }

    fn result_entry_id(&self, call_id: &ToolCallId) -> Option<EntryId> {
        lock_unpoisoned(&self.plan)
            .as_ref()
            .and_then(|plan| plan.calls.get(call_id))
            .filter(|intent| intent.state == DurableToolStartState::Persisted)
            .map(|intent| intent.result_entry_id.clone())
    }

    fn remember_error(&self, error: HarnessError) {
        *lock_unpoisoned(&self.error) = Some(error);
    }

    fn take_error(&self) -> Option<HarnessError> {
        lock_unpoisoned(&self.error).take()
    }
}

struct DurableSendToolPolicy {
    inner: Arc<dyn ToolPolicy>,
    session: Arc<Session>,
    state: Arc<SendDurableToolState>,
}

impl ToolPolicy for DurableSendToolPolicy {
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async move {
            let BeforeToolCall {
                assistant_message,
                tool_call,
                args,
                context,
            } = context;
            let authorization = self
                .inner
                .authorize(
                    BeforeToolCall {
                        assistant_message,
                        tool_call,
                        args: &mut *args,
                        context,
                    },
                    cancellation,
                )
                .await?;
            if authorization != ToolAuthorization::Allow {
                return Ok(authorization);
            }
            let (run_id, assistant_entry_id, intent) = self
                .state
                .intent(&assistant_message.id, tool_call)
                .map_err(|error| remember_send_tool_error(&self.state, error))?;
            if intent.state == DurableToolStartState::Pending {
                if let Err(error) = self
                    .session
                    .append_tool_started(
                        run_id,
                        assistant_entry_id,
                        intent.tool_index,
                        intent.call,
                        args.clone(),
                        intent.result_entry_id,
                        intent.replay,
                    )
                    .await
                    .map_err(HarnessError::from)
                {
                    return Err(remember_send_tool_error(&self.state, error));
                }
                self.state
                    .mark_persisted(&assistant_message.id, &tool_call.id);
            }
            Ok(authorization)
        })
    }

    fn finalize<'a>(
        &'a self,
        context: AfterToolCall<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        self.inner.finalize(context, cancellation)
    }
}

fn remember_send_tool_error(state: &SendDurableToolState, error: HarnessError) -> AgentError {
    let message = error.to_string();
    state.remember_error(error);
    AgentError::InvariantViolation { message }
}

#[derive(Default)]
struct LocalDurableToolState {
    plan: RefCell<Option<DurableToolPlan>>,
    error: RefCell<Option<HarnessError>>,
}

impl LocalDurableToolState {
    fn install(&self, plan: DurableToolPlan) {
        *self.plan.borrow_mut() = Some(plan);
        *self.error.borrow_mut() = None;
    }

    fn intent(
        &self,
        assistant_message_id: &MessageId,
        call: &ToolCall,
    ) -> Result<(RunId, EntryId, DurableToolStartIntent), HarnessError> {
        let plan = self.plan.borrow();
        let plan = plan
            .as_ref()
            .filter(|plan| &plan.assistant_message_id == assistant_message_id)
            .ok_or(HarnessError::IncompleteAgentStream)?;
        let intent = plan
            .calls
            .get(&call.id)
            .filter(|intent| intent.call.name == call.name)
            .cloned()
            .ok_or(HarnessError::IncompleteAgentStream)?;
        Ok((plan.run_id.clone(), plan.assistant_entry_id.clone(), intent))
    }

    fn mark_persisted(&self, assistant_message_id: &MessageId, call_id: &ToolCallId) {
        let mut plan = self.plan.borrow_mut();
        if let Some(plan) = plan.as_mut()
            && &plan.assistant_message_id == assistant_message_id
            && let Some(intent) = plan.calls.get_mut(call_id)
        {
            intent.state = DurableToolStartState::Persisted;
        }
    }

    fn result_entry_id(&self, call_id: &ToolCallId) -> Option<EntryId> {
        self.plan
            .borrow()
            .as_ref()
            .and_then(|plan| plan.calls.get(call_id))
            .filter(|intent| intent.state == DurableToolStartState::Persisted)
            .map(|intent| intent.result_entry_id.clone())
    }

    fn remember_error(&self, error: HarnessError) {
        *self.error.borrow_mut() = Some(error);
    }

    fn take_error(&self) -> Option<HarnessError> {
        self.error.borrow_mut().take()
    }
}

struct DurableLocalToolPolicy {
    inner: Rc<dyn LocalToolPolicy>,
    session: Rc<LocalSession>,
    state: Rc<LocalDurableToolState>,
}

impl LocalToolPolicy for DurableLocalToolPolicy {
    fn authorize<'a>(
        &'a self,
        context: LocalBeforeToolCall<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async move {
            let LocalBeforeToolCall {
                assistant_message,
                tool_call,
                args,
                context,
            } = context;
            let authorization = self
                .inner
                .authorize(
                    LocalBeforeToolCall {
                        assistant_message,
                        tool_call,
                        args: &mut *args,
                        context,
                    },
                    cancellation,
                )
                .await?;
            if authorization != ToolAuthorization::Allow {
                return Ok(authorization);
            }
            let (run_id, assistant_entry_id, intent) = self
                .state
                .intent(&assistant_message.id, tool_call)
                .map_err(|error| remember_local_tool_error(&self.state, error))?;
            if intent.state == DurableToolStartState::Pending {
                if let Err(error) = self
                    .session
                    .append_tool_started(
                        run_id,
                        assistant_entry_id,
                        intent.tool_index,
                        intent.call,
                        args.clone(),
                        intent.result_entry_id,
                        intent.replay,
                    )
                    .await
                    .map_err(HarnessError::from)
                {
                    return Err(remember_local_tool_error(&self.state, error));
                }
                self.state
                    .mark_persisted(&assistant_message.id, &tool_call.id);
            }
            Ok(authorization)
        })
    }

    fn finalize<'a>(
        &'a self,
        context: LocalAfterToolCall<'a>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        self.inner.finalize(context, cancellation)
    }
}

fn remember_local_tool_error(state: &LocalDurableToolState, error: HarnessError) -> AgentError {
    let message = error.to_string();
    state.remember_error(error);
    AgentError::InvariantViolation { message }
}

/// Cloneable durable queue-ingress and cancellation capability.
#[derive(Clone)]
pub struct HarnessControl {
    session: Arc<Session>,
    agent: AgentControl,
    active: Arc<Mutex<Option<ActiveHarnessRun>>>,
    transitions: Arc<AsyncMutex<()>>,
}

impl HarnessControl {
    /// Durably enqueues steering, then acknowledges the core in-memory queue.
    pub async fn steer(&self, message: AgentRecord) -> Result<DurableQueueReceipt, HarnessError> {
        self.enqueue_active(QueueKind::Steer, message).await
    }

    /// Durably enqueues follow-up input, then acknowledges the core queue.
    pub async fn follow_up(
        &self,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        self.enqueue_active(QueueKind::FollowUp, message).await
    }

    /// Durably queues input for the next prompt run.
    pub async fn next_run(
        &self,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        let _transition = self.transitions.lock().await;
        let target = provision_record(&self.session, "next-run", message);
        let entry_id = target.id().clone();
        let durable = self
            .session
            .enqueue(None, QueueKind::NextRun, target)
            .await?;
        Ok(DurableQueueReceipt {
            entry_id,
            queue: QueueKind::NextRun,
            durable,
            agent: None,
        })
    }

    /// Persists an abort request before signalling the active core run.
    pub async fn cancel(&self) -> Result<RunId, HarnessError> {
        let _transition = self.transitions.lock().await;
        let active = self.active_run()?;
        self.session
            .request_abort(active.operation_run_id.clone())
            .await?;
        self.agent.cancel(active.agent_run_id)?;
        Ok(active.operation_run_id)
    }

    async fn enqueue_active(
        &self,
        queue: QueueKind,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        let _transition = self.transitions.lock().await;
        let active = self.active_run()?;
        let target = provision_record(&self.session, "queued", message.clone());
        let entry_id = target.id().clone();
        let durable = self
            .session
            .enqueue(Some(active.operation_run_id.clone()), queue, target)
            .await?;
        let still_active = self.current_run().is_some_and(|current| {
            current.accepting_ingress
                && current.operation_run_id == active.operation_run_id
                && current.agent_run_id == active.agent_run_id
        });
        if !still_active {
            self.session
                .cancel_queued(Some(active.operation_run_id.clone()), entry_id.clone())
                .await?;
            return Err(HarnessError::OperationChanged {
                expected: active.operation_run_id,
            });
        }
        let accepted = match queue {
            QueueKind::Steer => self.agent.steer(message).await,
            QueueKind::FollowUp => self.agent.follow_up(message).await,
            QueueKind::NextRun => unreachable!("next-run has no core queue"),
        };
        let accepted = match accepted {
            Ok(receipt) => receipt,
            Err(error) => {
                self.session
                    .cancel_queued(Some(active.operation_run_id), entry_id)
                    .await?;
                return Err(error.into());
            }
        };
        Ok(DurableQueueReceipt {
            entry_id,
            queue,
            durable,
            agent: Some(accepted),
        })
    }

    fn active_run(&self) -> Result<ActiveHarnessRun, HarnessError> {
        self.current_run()
            .filter(|active| active.accepting_ingress)
            .ok_or_else(|| HarnessError::NoActiveRun {
                lane: self.session.lane().clone(),
            })
    }

    fn current_run(&self) -> Option<ActiveHarnessRun> {
        lock_unpoisoned(&self.active).clone()
    }
}

/// Send-capable durable harness around one agent and one session lane.
pub struct AgentHarness {
    agent: Agent,
    session: Arc<Session>,
    recovery: RecoveryDecision,
    tool_durability: Arc<SendDurableToolState>,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    events: HarnessEventBus,
    resume_hook: Arc<dyn RunResumeHook>,
    active: Arc<Mutex<Option<ActiveHarnessRun>>>,
    transitions: Arc<AsyncMutex<()>>,
}

impl AgentHarness {
    /// Opens a harness, reconstructing durable branch state and classifying any
    /// interrupted operation before accepting new work.
    pub async fn open(session: Arc<Session>, mut agent: Agent) -> Result<Self, HarnessError> {
        let state = session.load_state().await?;
        validate_recovery_log(&state, session.lane())?;
        let recovery = state.recovery_decision(session.lane());
        if let RecoveryDecision::Corrupt { open_operations } = &recovery {
            return Err(HarnessError::CorruptOpenOperations {
                open_operations: open_operations.clone(),
            });
        }
        restore_agent_from_session(&session, &mut agent).await?;
        restore_pending_core_queues(&state, session.lane(), &agent.control()).await?;
        let tool_durability = Arc::new(SendDurableToolState::default());
        let inner_tool_policy = agent.tool_scheduler().policy().clone();
        agent.set_tool_policy(Arc::new(DurableSendToolPolicy {
            inner: inner_tool_policy,
            session: session.clone(),
            state: tool_durability.clone(),
        }))?;
        Ok(Self {
            agent,
            session,
            recovery,
            tool_durability,
            tool_replay_policies: BTreeMap::new(),
            events: HarnessEventBus::new(),
            resume_hook: Arc::new(NoopRunResumeHook),
            active: Arc::new(Mutex::new(None)),
            transitions: Arc::new(AsyncMutex::new(())),
        })
    }

    /// Returns the selected lane's recovery classification observed at open.
    pub fn recovery(&self) -> &RecoveryDecision {
        &self.recovery
    }

    /// Returns the nested core agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Returns the selected durable session facade.
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Returns the passive harness event registry.
    pub fn events(&self) -> HarnessEventBus {
        self.events.clone()
    }

    /// Configures whether a started invocation of one tool may be replayed
    /// after crash recovery. Unconfigured tools default to
    /// [`ToolReplayPolicy::Never`], matching pinned Pi's `HarnessTool.replay`
    /// default.
    pub fn set_tool_replay_policy(
        &mut self,
        tool_name: impl Into<String>,
        replay: ToolReplayPolicy,
    ) {
        self.tool_replay_policies.insert(tool_name.into(), replay);
    }

    /// Installs the before-resume hook that owns persisted `resume_data`.
    pub fn set_run_resume_hook(&mut self, hook: Arc<dyn RunResumeHook>) {
        self.resume_hook = hook;
    }

    /// Returns the configured crash-replay policy for one model-facing tool.
    pub fn tool_replay_policy(&self, tool_name: &str) -> ToolReplayPolicy {
        configured_tool_replay_policy(&self.tool_replay_policies, tool_name)
    }

    /// Returns a cloneable durable control capability.
    pub fn control(&self) -> HarnessControl {
        HarnessControl {
            session: self.session.clone(),
            agent: self.agent.control(),
            active: self.active.clone(),
            transitions: self.transitions.clone(),
        }
    }

    /// Starts a prompt run after accepting the operation intent and initial
    /// entries durably. Pending `next_run` input precedes caller input.
    pub async fn prompt_records<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        cancellation: CancellationToken,
    ) -> Result<HarnessRunStream<'a>, HarnessError> {
        let transitions = self.transitions.clone();
        let transition = transitions.lock().await;
        let state = self.session.load_state().await?;
        let pending_next = pending_queue_items(&state, self.session.lane())
            .into_iter()
            .filter(|item| item.queue == QueueKind::NextRun)
            .collect::<Vec<_>>();
        let mut initial_messages = pending_next
            .iter()
            .map(|item| item.target.clone())
            .collect::<Vec<_>>();
        let caller_targets = records
            .iter()
            .cloned()
            .map(|record| provision_record(&self.session, "prompt", record))
            .collect::<Vec<_>>();
        initial_messages.extend(caller_targets);
        let mut initial_records = pending_next
            .iter()
            .filter_map(|item| provisioned_record(&item.target).cloned())
            .collect::<Vec<_>>();
        initial_records.extend(records.iter().cloned());
        let intent = OperationIntent::Run {
            original_prompt: records,
            initial_messages: initial_messages.clone(),
            system_prompt_override: None,
            resume_data: BTreeMap::new(),
        };
        let operation_run_id = self.session.start_operation(intent).await?;
        drop(transition);
        if let Err(error) = self
            .session
            .commit_provisioned_entries(initial_messages)
            .await
        {
            return Err(error.into());
        }
        let core_control = self.agent.control();
        let core = self
            .agent
            .prompt_records(initial_records.clone(), cancellation);
        Ok(drive_send_run(
            core,
            core_control,
            self.session.clone(),
            self.events.clone(),
            self.active.clone(),
            self.transitions.clone(),
            operation_run_id,
            initial_records,
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Constructs pinned Pi's text-then-images user message and starts a run.
    pub async fn prompt_text<'a>(
        &'a mut self,
        prompt: PromptText,
        cancellation: CancellationToken,
    ) -> Result<HarnessRunStream<'a>, HarnessError> {
        let id = self.session.next_entry_id("user-message");
        let message_id = MessageId::new(id.as_str());
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
        self.prompt_records(
            vec![AgentRecord::Llm(Message::User(UserMessage {
                id: message_id,
                content,
                timestamp: Timestamp::default(),
            }))],
            cancellation,
        )
        .await
    }

    /// Continues from the current durable tail under a new run operation.
    pub async fn continue_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<HarnessRunStream<'a>, HarnessError> {
        let core_control = self.agent.control();
        let core = self.agent.continue_run(cancellation)?;
        let operation_run_id = self.session.start_operation(empty_run_intent()).await?;
        Ok(drive_send_run(
            core,
            core_control,
            self.session.clone(),
            self.events.clone(),
            self.active.clone(),
            self.transitions.clone(),
            operation_run_id,
            Vec::new(),
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Retries the request boundary preceding a failed durable assistant.
    pub async fn retry_last_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<HarnessRunStream<'a>, HarnessError> {
        let core_control = self.agent.control();
        let core = self.agent.retry_last_turn(cancellation)?;
        let operation_run_id = self.session.start_operation(empty_run_intent()).await?;
        Ok(drive_send_run(
            core,
            core_control,
            self.session.clone(),
            self.events.clone(),
            self.active.clone(),
            self.transitions.clone(),
            operation_run_id,
            Vec::new(),
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Resumes an interrupted run intent detected by [`Self::open`].
    ///
    /// Missing initial entries are committed and emitted through the core. If
    /// all input was already committed, continuation or failed-turn retry is
    /// selected from the reconstructed durable tail. Tool-side-effect replay
    /// remains governed by durable `ToolReplayPolicy` records.
    pub async fn resume_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<HarnessRunStream<'a>, HarnessError> {
        let RecoveryDecision::Resume {
            operation,
            completed_steps,
        } = self.recovery.clone()
        else {
            return Err(HarnessError::UnsupportedRecovery {
                operation: "idle_or_abandoned".to_owned(),
            });
        };
        let operation_run_id = operation
            .run_id()
            .ok_or(HarnessError::InvalidRecoveryRecord)?;
        let resume_intent = run_resume_intent(&operation)?;
        self.resume_hook
            .before_resume(&resume_intent, cancellation.clone())
            .await?;
        let state = self.session.load_state().await?;
        let plan =
            derive_run_recovery_plan(&state, self.session.lane(), &operation, &completed_steps)?;
        match plan {
            RunRecoveryPlan::CommitMissing { missing, run_state } => {
                self.session
                    .commit_provisioned_entries(missing.clone())
                    .await?;
                let missing_records = missing
                    .iter()
                    .filter_map(|target| provisioned_record(target).cloned())
                    .collect::<Vec<_>>();
                let core_control = self.agent.control();
                let core = self.agent.resume_interrupted_turn(
                    missing_records.clone(),
                    true,
                    run_state,
                    cancellation,
                );
                Ok(drive_send_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    missing_records,
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::Continue(run_state) => {
                let core_control = self.agent.control();
                let core =
                    self.agent
                        .resume_interrupted_turn(Vec::new(), false, run_state, cancellation);
                Ok(drive_send_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::RecoverTools(batch) => {
                let recovery = recover_send_tool_batch(
                    &self.agent,
                    &self.session,
                    &operation_run_id,
                    *batch,
                    &self.tool_replay_policies,
                    self.tool_durability.clone(),
                    cancellation.clone(),
                )?;
                let core_control = self.agent.control();
                let core = self
                    .agent
                    .resume_completed_turn_stream(recovery, cancellation);
                Ok(drive_send_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::ResumeCompleted(turn) => {
                let core_control = self.agent.control();
                let core = self.agent.resume_completed_turn(*turn, cancellation);
                Ok(drive_send_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
        }
    }

    /// Explicitly closes an interrupted operation without resuming it.
    pub async fn abandon_recovery(
        &mut self,
        reason: PublicError,
    ) -> Result<AppendReceipt, HarnessError> {
        let operation = match &self.recovery {
            RecoveryDecision::Resume { operation, .. }
            | RecoveryDecision::Abandon { operation, .. } => operation,
            RecoveryDecision::Idle => {
                return Err(HarnessError::UnsupportedRecovery {
                    operation: "idle".to_owned(),
                });
            }
            RecoveryDecision::Corrupt { open_operations } => {
                return Err(HarnessError::CorruptOpenOperations {
                    open_operations: open_operations.clone(),
                });
            }
        };
        let run_id = operation
            .run_id()
            .ok_or(HarnessError::InvalidRecoveryRecord)?;
        let outcome = if matches!(self.recovery, RecoveryDecision::Abandon { .. }) {
            OperationOutcome::Aborted
        } else {
            OperationOutcome::Failed
        };
        let _transition = self.transitions.lock().await;
        let receipt = self
            .session
            .finish_operation(run_id, outcome, Some(reason))
            .await?;
        self.agent.clear_all_queues();
        self.recovery = self
            .session
            .load_state()
            .await?
            .recovery_decision(self.session.lane());
        Ok(receipt)
    }
}

/// Cloneable Local/WASM durable queue-ingress and cancellation capability.
#[derive(Clone)]
pub struct LocalHarnessControl {
    session: Rc<LocalSession>,
    agent: AgentControl,
    active: Rc<RefCell<Option<ActiveHarnessRun>>>,
    transitions: Rc<AsyncMutex<()>>,
}

impl LocalHarnessControl {
    /// Local counterpart of [`HarnessControl::steer`].
    pub async fn steer(&self, message: AgentRecord) -> Result<DurableQueueReceipt, HarnessError> {
        self.enqueue_active(QueueKind::Steer, message).await
    }

    /// Local counterpart of [`HarnessControl::follow_up`].
    pub async fn follow_up(
        &self,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        self.enqueue_active(QueueKind::FollowUp, message).await
    }

    /// Local counterpart of [`HarnessControl::next_run`].
    pub async fn next_run(
        &self,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        let _transition = self.transitions.lock().await;
        let target = provision_local_record(&self.session, "next-run", message);
        let entry_id = target.id().clone();
        let durable = self
            .session
            .enqueue(None, QueueKind::NextRun, target)
            .await?;
        Ok(DurableQueueReceipt {
            entry_id,
            queue: QueueKind::NextRun,
            durable,
            agent: None,
        })
    }

    /// Local counterpart of [`HarnessControl::cancel`].
    pub async fn cancel(&self) -> Result<RunId, HarnessError> {
        let _transition = self.transitions.lock().await;
        let active = self.active_run()?;
        self.session
            .request_abort(active.operation_run_id.clone())
            .await?;
        self.agent.cancel(active.agent_run_id)?;
        Ok(active.operation_run_id)
    }

    async fn enqueue_active(
        &self,
        queue: QueueKind,
        message: AgentRecord,
    ) -> Result<DurableQueueReceipt, HarnessError> {
        let _transition = self.transitions.lock().await;
        let active = self.active_run()?;
        let target = provision_local_record(&self.session, "queued", message.clone());
        let entry_id = target.id().clone();
        let durable = self
            .session
            .enqueue(Some(active.operation_run_id.clone()), queue, target)
            .await?;
        let still_active = self.current_run().is_some_and(|current| {
            current.accepting_ingress
                && current.operation_run_id == active.operation_run_id
                && current.agent_run_id == active.agent_run_id
        });
        if !still_active {
            self.session
                .cancel_queued(Some(active.operation_run_id.clone()), entry_id.clone())
                .await?;
            return Err(HarnessError::OperationChanged {
                expected: active.operation_run_id,
            });
        }
        let accepted = match queue {
            QueueKind::Steer => self.agent.steer(message).await,
            QueueKind::FollowUp => self.agent.follow_up(message).await,
            QueueKind::NextRun => unreachable!("next-run has no core queue"),
        };
        let accepted = match accepted {
            Ok(receipt) => receipt,
            Err(error) => {
                self.session
                    .cancel_queued(Some(active.operation_run_id), entry_id)
                    .await?;
                return Err(error.into());
            }
        };
        Ok(DurableQueueReceipt {
            entry_id,
            queue,
            durable,
            agent: Some(accepted),
        })
    }

    fn active_run(&self) -> Result<ActiveHarnessRun, HarnessError> {
        self.current_run()
            .filter(|active| active.accepting_ingress)
            .ok_or_else(|| HarnessError::NoActiveRun {
                lane: self.session.lane().clone(),
            })
    }

    fn current_run(&self) -> Option<ActiveHarnessRun> {
        self.active.borrow().clone()
    }
}

/// Local/WASM durable harness retaining `Rc`-owned runtime and storage state.
pub struct LocalAgentHarness {
    agent: LocalAgent,
    session: Rc<LocalSession>,
    recovery: RecoveryDecision,
    tool_durability: Rc<LocalDurableToolState>,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    events: LocalHarnessEventBus,
    resume_hook: Rc<dyn LocalRunResumeHook>,
    active: Rc<RefCell<Option<ActiveHarnessRun>>>,
    transitions: Rc<AsyncMutex<()>>,
}

impl LocalAgentHarness {
    /// Opens and reconstructs a local durable harness.
    pub async fn open(
        session: Rc<LocalSession>,
        mut agent: LocalAgent,
    ) -> Result<Self, HarnessError> {
        let state = session.load_state().await?;
        validate_recovery_log(&state, session.lane())?;
        let recovery = state.recovery_decision(session.lane());
        if let RecoveryDecision::Corrupt { open_operations } = &recovery {
            return Err(HarnessError::CorruptOpenOperations {
                open_operations: open_operations.clone(),
            });
        }
        restore_local_agent_from_session(&session, &mut agent).await?;
        restore_pending_core_queues(&state, session.lane(), &agent.control()).await?;
        let tool_durability = Rc::new(LocalDurableToolState::default());
        let inner_tool_policy = agent.tool_scheduler().policy().clone();
        agent.set_tool_policy(Rc::new(DurableLocalToolPolicy {
            inner: inner_tool_policy,
            session: session.clone(),
            state: tool_durability.clone(),
        }))?;
        Ok(Self {
            agent,
            session,
            recovery,
            tool_durability,
            tool_replay_policies: BTreeMap::new(),
            events: LocalHarnessEventBus::new(),
            resume_hook: Rc::new(NoopRunResumeHook),
            active: Rc::new(RefCell::new(None)),
            transitions: Rc::new(AsyncMutex::new(())),
        })
    }

    /// Returns the recovery classification observed at open.
    pub fn recovery(&self) -> &RecoveryDecision {
        &self.recovery
    }

    /// Returns the nested local core agent.
    pub fn agent(&self) -> &LocalAgent {
        &self.agent
    }

    /// Returns the selected local durable session facade.
    pub fn session(&self) -> &Rc<LocalSession> {
        &self.session
    }

    /// Returns the passive harness event registry.
    pub fn events(&self) -> LocalHarnessEventBus {
        self.events.clone()
    }

    /// Local counterpart of [`AgentHarness::set_tool_replay_policy`].
    pub fn set_tool_replay_policy(
        &mut self,
        tool_name: impl Into<String>,
        replay: ToolReplayPolicy,
    ) {
        self.tool_replay_policies.insert(tool_name.into(), replay);
    }

    /// Installs the local before-resume hook that owns persisted `resume_data`.
    pub fn set_run_resume_hook(&mut self, hook: Rc<dyn LocalRunResumeHook>) {
        self.resume_hook = hook;
    }

    /// Local counterpart of [`AgentHarness::tool_replay_policy`].
    pub fn tool_replay_policy(&self, tool_name: &str) -> ToolReplayPolicy {
        configured_tool_replay_policy(&self.tool_replay_policies, tool_name)
    }

    /// Returns a cloneable local durable control capability.
    pub fn control(&self) -> LocalHarnessControl {
        LocalHarnessControl {
            session: self.session.clone(),
            agent: self.agent.control(),
            active: self.active.clone(),
            transitions: self.transitions.clone(),
        }
    }

    /// Local counterpart of [`AgentHarness::prompt_records`].
    pub async fn prompt_records<'a>(
        &'a mut self,
        records: Vec<AgentRecord>,
        cancellation: CancellationToken,
    ) -> Result<LocalHarnessRunStream<'a>, HarnessError> {
        let transitions = self.transitions.clone();
        let transition = transitions.lock().await;
        let state = self.session.load_state().await?;
        let pending_next = pending_queue_items(&state, self.session.lane())
            .into_iter()
            .filter(|item| item.queue == QueueKind::NextRun)
            .collect::<Vec<_>>();
        let mut initial_messages = pending_next
            .iter()
            .map(|item| item.target.clone())
            .collect::<Vec<_>>();
        initial_messages.extend(
            records
                .iter()
                .cloned()
                .map(|record| provision_local_record(&self.session, "prompt", record)),
        );
        let mut initial_records = pending_next
            .iter()
            .filter_map(|item| provisioned_record(&item.target).cloned())
            .collect::<Vec<_>>();
        initial_records.extend(records.iter().cloned());
        let operation_run_id = self
            .session
            .start_operation(OperationIntent::Run {
                original_prompt: records,
                initial_messages: initial_messages.clone(),
                system_prompt_override: None,
                resume_data: BTreeMap::new(),
            })
            .await?;
        drop(transition);
        self.session
            .commit_provisioned_entries(initial_messages)
            .await?;
        let core_control = self.agent.control();
        let core = self
            .agent
            .prompt_records(initial_records.clone(), cancellation);
        Ok(drive_local_run(
            core,
            core_control,
            self.session.clone(),
            self.events.clone(),
            self.active.clone(),
            self.transitions.clone(),
            operation_run_id,
            initial_records,
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Local counterpart of [`AgentHarness::prompt_text`].
    pub async fn prompt_text<'a>(
        &'a mut self,
        prompt: PromptText,
        cancellation: CancellationToken,
    ) -> Result<LocalHarnessRunStream<'a>, HarnessError> {
        let id = self.session.next_entry_id("user-message");
        let message_id = MessageId::new(id.as_str());
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
        self.prompt_records(
            vec![AgentRecord::Llm(Message::User(UserMessage {
                id: message_id,
                content,
                timestamp: Timestamp::default(),
            }))],
            cancellation,
        )
        .await
    }

    /// Local counterpart of [`AgentHarness::continue_run`].
    pub async fn continue_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<LocalHarnessRunStream<'a>, HarnessError> {
        let session = self.session.clone();
        let events = self.events.clone();
        let active = self.active.clone();
        let transitions = self.transitions.clone();
        let core_control = self.agent.control();
        let core = self.agent.continue_run(cancellation)?;
        let operation_run_id = session.start_operation(empty_run_intent()).await?;
        Ok(drive_local_run(
            core,
            core_control,
            session,
            events,
            active,
            transitions,
            operation_run_id,
            Vec::new(),
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Local counterpart of [`AgentHarness::retry_last_turn`].
    pub async fn retry_last_turn<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<LocalHarnessRunStream<'a>, HarnessError> {
        let session = self.session.clone();
        let events = self.events.clone();
        let active = self.active.clone();
        let transitions = self.transitions.clone();
        let core_control = self.agent.control();
        let core = self.agent.retry_last_turn(cancellation)?;
        let operation_run_id = session.start_operation(empty_run_intent()).await?;
        Ok(drive_local_run(
            core,
            core_control,
            session,
            events,
            active,
            transitions,
            operation_run_id,
            Vec::new(),
            self.tool_replay_policies.clone(),
            self.tool_durability.clone(),
            None,
        ))
    }

    /// Local counterpart of [`AgentHarness::resume_run`].
    pub async fn resume_run<'a>(
        &'a mut self,
        cancellation: CancellationToken,
    ) -> Result<LocalHarnessRunStream<'a>, HarnessError> {
        let RecoveryDecision::Resume {
            operation,
            completed_steps,
        } = self.recovery.clone()
        else {
            return Err(HarnessError::UnsupportedRecovery {
                operation: "idle_or_abandoned".to_owned(),
            });
        };
        let operation_run_id = operation
            .run_id()
            .ok_or(HarnessError::InvalidRecoveryRecord)?;
        let resume_intent = run_resume_intent(&operation)?;
        self.resume_hook
            .before_resume(&resume_intent, cancellation.clone())
            .await?;
        let state = self.session.load_state().await?;
        let plan =
            derive_run_recovery_plan(&state, self.session.lane(), &operation, &completed_steps)?;
        match plan {
            RunRecoveryPlan::CommitMissing { missing, run_state } => {
                self.session
                    .commit_provisioned_entries(missing.clone())
                    .await?;
                let missing_records = missing
                    .iter()
                    .filter_map(|target| provisioned_record(target).cloned())
                    .collect::<Vec<_>>();
                let core_control = self.agent.control();
                let core = self.agent.resume_interrupted_turn(
                    missing_records.clone(),
                    true,
                    run_state,
                    cancellation,
                );
                Ok(drive_local_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    missing_records,
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::Continue(run_state) => {
                let core_control = self.agent.control();
                let core =
                    self.agent
                        .resume_interrupted_turn(Vec::new(), false, run_state, cancellation);
                Ok(drive_local_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::RecoverTools(batch) => {
                let recovery = recover_local_tool_batch(
                    &self.agent,
                    &self.session,
                    &operation_run_id,
                    *batch,
                    &self.tool_replay_policies,
                    self.tool_durability.clone(),
                    cancellation.clone(),
                )?;
                let core_control = self.agent.control();
                let core = self
                    .agent
                    .resume_completed_turn_stream(recovery, cancellation);
                Ok(drive_local_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
            RunRecoveryPlan::ResumeCompleted(turn) => {
                let core_control = self.agent.control();
                let core = self.agent.resume_completed_turn(*turn, cancellation);
                Ok(drive_local_run(
                    core,
                    core_control,
                    self.session.clone(),
                    self.events.clone(),
                    self.active.clone(),
                    self.transitions.clone(),
                    operation_run_id,
                    Vec::new(),
                    self.tool_replay_policies.clone(),
                    self.tool_durability.clone(),
                    Some(&mut self.recovery),
                ))
            }
        }
    }

    /// Local counterpart of [`AgentHarness::abandon_recovery`].
    pub async fn abandon_recovery(
        &mut self,
        reason: PublicError,
    ) -> Result<AppendReceipt, HarnessError> {
        let operation = match &self.recovery {
            RecoveryDecision::Resume { operation, .. }
            | RecoveryDecision::Abandon { operation, .. } => operation,
            RecoveryDecision::Idle => {
                return Err(HarnessError::UnsupportedRecovery {
                    operation: "idle".to_owned(),
                });
            }
            RecoveryDecision::Corrupt { open_operations } => {
                return Err(HarnessError::CorruptOpenOperations {
                    open_operations: open_operations.clone(),
                });
            }
        };
        let run_id = operation
            .run_id()
            .ok_or(HarnessError::InvalidRecoveryRecord)?;
        let outcome = if matches!(self.recovery, RecoveryDecision::Abandon { .. }) {
            OperationOutcome::Aborted
        } else {
            OperationOutcome::Failed
        };
        let _transition = self.transitions.lock().await;
        let receipt = self
            .session
            .finish_operation(run_id, outcome, Some(reason))
            .await?;
        self.agent.clear_all_queues();
        self.recovery = self
            .session
            .load_state()
            .await?
            .recovery_decision(self.session.lane());
        Ok(receipt)
    }
}

struct SendDriverState {
    session: Arc<Session>,
    operation_run_id: RunId,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Arc<SendDurableToolState>,
    initial_commits: VecDeque<AgentRecord>,
    pending_assistant: Option<(EntryId, u32)>,
    tool_termination: BTreeMap<ToolCallId, bool>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "driver inputs keep Send capabilities and borrowed recovery state explicit"
)]
fn drive_send_run<'a>(
    mut core: SendBoxStream<'a, AgentEvent>,
    core_control: AgentControl,
    session: Arc<Session>,
    events: HarnessEventBus,
    active: Arc<Mutex<Option<ActiveHarnessRun>>>,
    transitions: Arc<AsyncMutex<()>>,
    operation_run_id: RunId,
    initial_commits: Vec<AgentRecord>,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Arc<SendDurableToolState>,
    mut recovery: Option<&'a mut RecoveryDecision>,
) -> HarnessRunStream<'a> {
    Box::pin(async_stream::stream! {
        let Some(first) = core.next().await else {
            yield Err(HarnessError::IncompleteAgentStream);
            return;
        };
        let AgentEvent::RunStarted { run_id: agent_run_id } = &first else {
            yield Err(HarnessError::IncompleteAgentStream);
            return;
        };
        let mut active_guard = ActiveRunGuard::new(
            active,
            ActiveHarnessRun {
                operation_run_id: operation_run_id.clone(),
                agent_run_id: agent_run_id.clone(),
                accepting_ingress: true,
            },
        );
        let start = HarnessEvent::RunStart {
            lane: session.lane().clone(),
            run_id: operation_run_id.clone(),
        };
        events.emit(start.clone());
        yield Ok(HarnessRunEvent::Harness(start));

        let mut driver = SendDriverState {
            session: session.clone(),
            operation_run_id: operation_run_id.clone(),
            tool_replay_policies,
            tool_durability,
            initial_commits: initial_commits.into(),
            pending_assistant: None,
            tool_termination: BTreeMap::new(),
        };
        let mut next = Some(first);
        while let Some(event) = next {
            if let Err(error) = process_send_event(&event, &mut driver).await {
                yield Err(error);
                return;
            }
            if let AgentEvent::RunFinished { outcome } = &event {
                active_guard.mark_closing();
                let _transition = transitions.lock().await;
                let (operation_outcome, harness_outcome, error) = operation_terminal(outcome);
                if let Err(error) = session
                    .finish_operation(operation_run_id.clone(), operation_outcome, error)
                    .await
                {
                    yield Err(error.into());
                    return;
                }
                if operation_outcome != OperationOutcome::Completed {
                    core_control.clear_all();
                }
                let state = match session.load_state().await {
                    Ok(state) => state,
                    Err(error) => {
                        yield Err(error.into());
                        return;
                    }
                };
                let Some(leaf_id) = state.lane_leaf(session.lane()).cloned().flatten() else {
                    yield Err(HarnessError::MissingRunLeaf {
                        run_id: operation_run_id.clone(),
                    });
                    return;
                };
                active_guard.finish();
                if let Some(recovery) = recovery.as_deref_mut() {
                    *recovery = RecoveryDecision::Idle;
                }
                yield Ok(HarnessRunEvent::Agent(Box::new(event)));
                let end = HarnessEvent::RunEnd {
                    lane: session.lane().clone(),
                    run_id: operation_run_id,
                    outcome: harness_outcome,
                    leaf_id,
                };
                events.emit(end.clone());
                yield Ok(HarnessRunEvent::Harness(end));
                return;
            }
            yield Ok(HarnessRunEvent::Agent(Box::new(event)));
            next = core.next().await;
        }
        yield Err(HarnessError::IncompleteAgentStream);
    })
}

async fn process_send_event(
    event: &AgentEvent,
    driver: &mut SendDriverState,
) -> Result<(), HarnessError> {
    if let Some(error) = driver.tool_durability.take_error() {
        return Err(error);
    }
    match event {
        AgentEvent::ContextPrepared { .. } => ensure_assistant_attempt(driver).await?,
        AgentEvent::MessageStarted {
            role: MessageRole::Assistant,
            ..
        } if driver.pending_assistant.is_none() => ensure_assistant_attempt(driver).await?,
        AgentEvent::MessageCommitted { message } => {
            if driver.initial_commits.front() == Some(message) {
                driver.initial_commits.pop_front();
                return Ok(());
            }
            match message {
                AgentRecord::Llm(Message::Assistant(assistant)) => {
                    ensure_assistant_attempt(driver).await?;
                    let (entry_id, attempt) = driver
                        .pending_assistant
                        .take()
                        .ok_or(HarnessError::IncompleteAgentStream)?;
                    driver
                        .session
                        .commit_assistant(
                            driver.operation_run_id.clone(),
                            attempt,
                            entry_id.clone(),
                            assistant.clone(),
                        )
                        .await?;
                    driver.tool_durability.install(live_durable_tool_plan(
                        &driver.session,
                        driver.operation_run_id.clone(),
                        entry_id,
                        assistant,
                        &driver.tool_replay_policies,
                    )?);
                }
                AgentRecord::Llm(Message::User(_))
                | AgentRecord::Llm(Message::ToolResult(_))
                | AgentRecord::Custom { .. } => {
                    let state = driver.session.load_state().await?;
                    let queued = pending_queue_items(&state, driver.session.lane())
                        .into_iter()
                        .find(|item| {
                            item.queue != QueueKind::NextRun
                                && provisioned_record(&item.target) == Some(message)
                        })
                        .map(|item| item.target);
                    let entry_id = queued
                        .as_ref()
                        .map(|target| target.id().clone())
                        .or_else(|| match message {
                            AgentRecord::Llm(Message::ToolResult(result)) => {
                                driver.tool_durability.result_entry_id(&result.tool_call_id)
                            }
                            AgentRecord::Llm(Message::User(_))
                            | AgentRecord::Custom { .. }
                            | AgentRecord::Llm(Message::Assistant(_)) => None,
                        })
                        .unwrap_or_else(|| driver.session.next_entry_id("message"));
                    let terminate = match message {
                        AgentRecord::Llm(Message::ToolResult(result)) => driver
                            .tool_termination
                            .get(&result.tool_call_id)
                            .copied()
                            .unwrap_or(false),
                        AgentRecord::Llm(Message::User(_))
                        | AgentRecord::Llm(Message::Assistant(_))
                        | AgentRecord::Custom { .. } => false,
                    };
                    driver
                        .session
                        .commit_agent_record(
                            driver.operation_run_id.clone(),
                            entry_id,
                            message.clone(),
                            terminate,
                        )
                        .await?;
                }
            }
        }
        AgentEvent::ToolExecutionFinished {
            call_id, result, ..
        } => {
            driver
                .tool_termination
                .insert(call_id.clone(), result.terminate);
        }
        AgentEvent::RunStarted { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::MessageStarted { .. }
        | AgentEvent::AssistantUpdate { .. }
        | AgentEvent::ToolExecutionStarted { .. }
        | AgentEvent::ToolExecutionUpdated { .. }
        | AgentEvent::TurnFinished { .. }
        | AgentEvent::RunFinished { .. } => {}
        _ => {}
    }
    Ok(())
}

async fn ensure_assistant_attempt(driver: &mut SendDriverState) -> Result<(), HarnessError> {
    if driver.pending_assistant.is_some() {
        return Ok(());
    }
    let state = driver.session.load_state().await?;
    let attempt = next_step_attempt(&state, &driver.operation_run_id, OperationStep::Assistant);
    let entry_id = driver.session.next_entry_id("assistant");
    driver
        .session
        .append_step_attempt(
            driver.operation_run_id.clone(),
            OperationStep::Assistant,
            attempt,
            entry_id.clone(),
            None,
        )
        .await?;
    driver.pending_assistant = Some((entry_id, attempt));
    Ok(())
}

struct LocalDriverState {
    session: Rc<LocalSession>,
    operation_run_id: RunId,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Rc<LocalDurableToolState>,
    initial_commits: VecDeque<AgentRecord>,
    pending_assistant: Option<(EntryId, u32)>,
    tool_termination: BTreeMap<ToolCallId, bool>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "driver inputs keep Local capabilities and borrowed recovery state explicit"
)]
fn drive_local_run<'a>(
    mut core: LocalBoxStream<'a, AgentEvent>,
    core_control: AgentControl,
    session: Rc<LocalSession>,
    events: LocalHarnessEventBus,
    active: Rc<RefCell<Option<ActiveHarnessRun>>>,
    transitions: Rc<AsyncMutex<()>>,
    operation_run_id: RunId,
    initial_commits: Vec<AgentRecord>,
    tool_replay_policies: BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Rc<LocalDurableToolState>,
    mut recovery: Option<&'a mut RecoveryDecision>,
) -> LocalHarnessRunStream<'a> {
    Box::pin(async_stream::stream! {
        let Some(first) = core.next().await else {
            yield Err(HarnessError::IncompleteAgentStream);
            return;
        };
        let AgentEvent::RunStarted { run_id: agent_run_id } = &first else {
            yield Err(HarnessError::IncompleteAgentStream);
            return;
        };
        let mut active_guard = LocalActiveRunGuard::new(
            active,
            ActiveHarnessRun {
                operation_run_id: operation_run_id.clone(),
                agent_run_id: agent_run_id.clone(),
                accepting_ingress: true,
            },
        );
        let start = HarnessEvent::RunStart {
            lane: session.lane().clone(),
            run_id: operation_run_id.clone(),
        };
        events.emit(start.clone());
        yield Ok(HarnessRunEvent::Harness(start));

        let mut driver = LocalDriverState {
            session: session.clone(),
            operation_run_id: operation_run_id.clone(),
            tool_replay_policies,
            tool_durability,
            initial_commits: initial_commits.into(),
            pending_assistant: None,
            tool_termination: BTreeMap::new(),
        };
        let mut next = Some(first);
        while let Some(event) = next {
            if let Err(error) = process_local_event(&event, &mut driver).await {
                yield Err(error);
                return;
            }
            if let AgentEvent::RunFinished { outcome } = &event {
                active_guard.mark_closing();
                let _transition = transitions.lock().await;
                let (operation_outcome, harness_outcome, error) = operation_terminal(outcome);
                if let Err(error) = session
                    .finish_operation(operation_run_id.clone(), operation_outcome, error)
                    .await
                {
                    yield Err(error.into());
                    return;
                }
                if operation_outcome != OperationOutcome::Completed {
                    core_control.clear_all();
                }
                let state = match session.load_state().await {
                    Ok(state) => state,
                    Err(error) => {
                        yield Err(error.into());
                        return;
                    }
                };
                let Some(leaf_id) = state.lane_leaf(session.lane()).cloned().flatten() else {
                    yield Err(HarnessError::MissingRunLeaf {
                        run_id: operation_run_id.clone(),
                    });
                    return;
                };
                active_guard.finish();
                if let Some(recovery) = recovery.as_deref_mut() {
                    *recovery = RecoveryDecision::Idle;
                }
                yield Ok(HarnessRunEvent::Agent(Box::new(event)));
                let end = HarnessEvent::RunEnd {
                    lane: session.lane().clone(),
                    run_id: operation_run_id,
                    outcome: harness_outcome,
                    leaf_id,
                };
                events.emit(end.clone());
                yield Ok(HarnessRunEvent::Harness(end));
                return;
            }
            yield Ok(HarnessRunEvent::Agent(Box::new(event)));
            next = core.next().await;
        }
        yield Err(HarnessError::IncompleteAgentStream);
    })
}

async fn process_local_event(
    event: &AgentEvent,
    driver: &mut LocalDriverState,
) -> Result<(), HarnessError> {
    if let Some(error) = driver.tool_durability.take_error() {
        return Err(error);
    }
    match event {
        AgentEvent::ContextPrepared { .. } => ensure_local_assistant_attempt(driver).await?,
        AgentEvent::MessageStarted {
            role: MessageRole::Assistant,
            ..
        } if driver.pending_assistant.is_none() => ensure_local_assistant_attempt(driver).await?,
        AgentEvent::MessageCommitted { message } => {
            if driver.initial_commits.front() == Some(message) {
                driver.initial_commits.pop_front();
                return Ok(());
            }
            match message {
                AgentRecord::Llm(Message::Assistant(assistant)) => {
                    ensure_local_assistant_attempt(driver).await?;
                    let (entry_id, attempt) = driver
                        .pending_assistant
                        .take()
                        .ok_or(HarnessError::IncompleteAgentStream)?;
                    driver
                        .session
                        .commit_assistant(
                            driver.operation_run_id.clone(),
                            attempt,
                            entry_id.clone(),
                            assistant.clone(),
                        )
                        .await?;
                    driver.tool_durability.install(local_live_durable_tool_plan(
                        &driver.session,
                        driver.operation_run_id.clone(),
                        entry_id,
                        assistant,
                        &driver.tool_replay_policies,
                    )?);
                }
                AgentRecord::Llm(Message::User(_))
                | AgentRecord::Llm(Message::ToolResult(_))
                | AgentRecord::Custom { .. } => {
                    let state = driver.session.load_state().await?;
                    let queued = pending_queue_items(&state, driver.session.lane())
                        .into_iter()
                        .find(|item| {
                            item.queue != QueueKind::NextRun
                                && provisioned_record(&item.target) == Some(message)
                        })
                        .map(|item| item.target);
                    let entry_id = queued
                        .as_ref()
                        .map(|target| target.id().clone())
                        .or_else(|| match message {
                            AgentRecord::Llm(Message::ToolResult(result)) => {
                                driver.tool_durability.result_entry_id(&result.tool_call_id)
                            }
                            AgentRecord::Llm(Message::User(_))
                            | AgentRecord::Custom { .. }
                            | AgentRecord::Llm(Message::Assistant(_)) => None,
                        })
                        .unwrap_or_else(|| driver.session.next_entry_id("message"));
                    let terminate = match message {
                        AgentRecord::Llm(Message::ToolResult(result)) => driver
                            .tool_termination
                            .get(&result.tool_call_id)
                            .copied()
                            .unwrap_or(false),
                        AgentRecord::Llm(Message::User(_))
                        | AgentRecord::Llm(Message::Assistant(_))
                        | AgentRecord::Custom { .. } => false,
                    };
                    driver
                        .session
                        .commit_agent_record(
                            driver.operation_run_id.clone(),
                            entry_id,
                            message.clone(),
                            terminate,
                        )
                        .await?;
                }
            }
        }
        AgentEvent::ToolExecutionFinished {
            call_id, result, ..
        } => {
            driver
                .tool_termination
                .insert(call_id.clone(), result.terminate);
        }
        AgentEvent::RunStarted { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::MessageStarted { .. }
        | AgentEvent::AssistantUpdate { .. }
        | AgentEvent::ToolExecutionStarted { .. }
        | AgentEvent::ToolExecutionUpdated { .. }
        | AgentEvent::TurnFinished { .. }
        | AgentEvent::RunFinished { .. } => {}
        _ => {}
    }
    Ok(())
}

async fn ensure_local_assistant_attempt(driver: &mut LocalDriverState) -> Result<(), HarnessError> {
    if driver.pending_assistant.is_some() {
        return Ok(());
    }
    let state = driver.session.load_state().await?;
    let attempt = next_step_attempt(&state, &driver.operation_run_id, OperationStep::Assistant);
    let entry_id = driver.session.next_entry_id("assistant");
    driver
        .session
        .append_step_attempt(
            driver.operation_run_id.clone(),
            OperationStep::Assistant,
            attempt,
            entry_id.clone(),
            None,
        )
        .await?;
    driver.pending_assistant = Some((entry_id, attempt));
    Ok(())
}

async fn restore_agent_from_session(
    session: &Session,
    agent: &mut Agent,
) -> Result<(), HarnessError> {
    let path = session.branch_entries().await?;
    let reconstructed =
        reconstruct_branch_context(&path).map_err(|error| HarnessError::Context {
            message: error.to_string(),
        })?;
    let state = agent.state_mut()?;
    state.transcript = reconstructed.records;
    if let Some(model) = reconstructed.model {
        state.model = model;
    }
    if reconstructed.reasoning_override.is_some() {
        state.reasoning = reconstructed.reasoning;
    }
    Ok(())
}

async fn restore_local_agent_from_session(
    session: &LocalSession,
    agent: &mut LocalAgent,
) -> Result<(), HarnessError> {
    let path = session.branch_entries().await?;
    let reconstructed =
        reconstruct_branch_context(&path).map_err(|error| HarnessError::Context {
            message: error.to_string(),
        })?;
    let state = agent.state_mut()?;
    state.transcript = reconstructed.records;
    if let Some(model) = reconstructed.model {
        state.model = model;
    }
    if reconstructed.reasoning_override.is_some() {
        state.reasoning = reconstructed.reasoning;
    }
    Ok(())
}

async fn restore_pending_core_queues(
    state: &SessionState,
    lane: &agentprism_session::LaneName,
    control: &AgentControl,
) -> Result<(), HarnessError> {
    for item in pending_queue_items(state, lane) {
        let Some(record) = provisioned_record(&item.target).cloned() else {
            continue;
        };
        match item.queue {
            QueueKind::Steer => {
                control.steer(record).await?;
            }
            QueueKind::FollowUp => {
                control.follow_up(record).await?;
            }
            QueueKind::NextRun => {}
        }
    }
    Ok(())
}

#[derive(Clone)]
struct PendingQueueItem {
    sequence: agentprism_session::Sequence,
    run_id: Option<RunId>,
    queue: QueueKind,
    target: ProvisionedEntry,
}

fn pending_queue_items(
    state: &SessionState,
    lane: &agentprism_session::LaneName,
) -> Vec<PendingQueueItem> {
    let mut pending = BTreeMap::<EntryId, PendingQueueItem>::new();
    for record in state.records_in_sequence_order() {
        if record.lane() != lane {
            continue;
        }
        match record {
            OperationRecord::QueueEnqueued {
                base,
                run_id,
                queue,
                target,
            } => {
                pending.insert(
                    target.id().clone(),
                    PendingQueueItem {
                        sequence: base.sequence,
                        run_id: run_id.clone(),
                        queue: *queue,
                        target: target.clone(),
                    },
                );
            }
            OperationRecord::QueueCancelled {
                run_id, entry_id, ..
            } if pending
                .get(entry_id)
                .is_some_and(|item| item.run_id.as_ref() == run_id.as_ref()) =>
            {
                pending.remove(entry_id);
            }
            OperationRecord::Started { .. }
            | OperationRecord::AbortRequested { .. }
            | OperationRecord::Finished { .. }
            | OperationRecord::StepAttempt { .. }
            | OperationRecord::ToolStarted { .. }
            | OperationRecord::QueueCancelled { .. }
            | OperationRecord::WriteDeferred { .. }
            | OperationRecord::Usage { .. } => {}
        }
    }
    let mut pending = pending
        .into_values()
        .filter(|item| state.entry(item.target.id()).is_none())
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence);
    pending
}

#[derive(Clone)]
struct RecoveryAttempt {
    step: OperationStep,
    attempt: u32,
    result_entry_id: EntryId,
    compaction_reason: Option<CompactionReason>,
}

fn corrupt_recovery(reason: RecoveryCorruptionReason, message: impl Into<String>) -> HarnessError {
    HarnessError::CorruptRecordLog {
        reason,
        message: message.into(),
    }
}

fn provisioned_entry_matches(entry: &SessionEntry, target: &ProvisionedEntry) -> bool {
    target.clone().materialize(
        entry.sequence(),
        entry.parent_id().cloned(),
        entry.base().timestamp,
    ) == *entry
}

fn validate_exact_provisioned_entry(
    state: &SessionState,
    target: &ProvisionedEntry,
) -> Result<(), HarnessError> {
    if let Some(entry) = state.entry(target.id())
        && !provisioned_entry_matches(entry, target)
    {
        return Err(corrupt_recovery(
            RecoveryCorruptionReason::ProvisionedEntryMismatch,
            format!(
                "provisioned entry {} exists with content different from its intent",
                target.id()
            ),
        ));
    }
    Ok(())
}

fn validate_result_entry(
    state: &SessionState,
    result_entry_id: &EntryId,
    matches: impl FnOnce(&SessionEntry) -> bool,
    description: &str,
) -> Result<(), HarnessError> {
    if let Some(entry) = state.entry(result_entry_id)
        && !matches(entry)
    {
        return Err(corrupt_recovery(
            RecoveryCorruptionReason::ProvisionedEntryMismatch,
            format!(
                "provisioned {description} entry {result_entry_id} exists with different content"
            ),
        ));
    }
    Ok(())
}

fn validate_operation_result(
    state: &SessionState,
    intent: &OperationIntent,
) -> Result<(), HarnessError> {
    match intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            for target in initial_messages {
                validate_exact_provisioned_entry(state, target)?;
            }
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => validate_result_entry(
            state,
            result_entry_id,
            |entry| matches!(entry, SessionEntry::Compaction { .. }),
            "manual compaction",
        )?,
        OperationIntent::Navigation {
            summary_entry_id: Some(summary_entry_id),
            ..
        } => validate_result_entry(
            state,
            summary_entry_id,
            |entry| matches!(entry, SessionEntry::BranchSummary { .. }),
            "navigation summary",
        )?,
        OperationIntent::Navigation {
            summary_entry_id: None,
            ..
        } => {}
    }
    Ok(())
}

fn validate_attempt_result(
    state: &SessionState,
    step: OperationStep,
    result_entry_id: &EntryId,
) -> Result<(), HarnessError> {
    match step {
        OperationStep::Assistant => validate_result_entry(
            state,
            result_entry_id,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::Message {
                        message: AgentRecord::Llm(Message::Assistant(_)),
                        ..
                    }
                )
            },
            "assistant result",
        ),
        OperationStep::Compaction => validate_result_entry(
            state,
            result_entry_id,
            |entry| matches!(entry, SessionEntry::Compaction { .. }),
            "compaction result",
        ),
        OperationStep::BranchSummary => validate_result_entry(
            state,
            result_entry_id,
            |entry| matches!(entry, SessionEntry::BranchSummary { .. }),
            "branch-summary result",
        ),
    }
}

fn validate_tool_start(
    state: &SessionState,
    record: &OperationRecord,
    invocations: &mut BTreeSet<(EntryId, u32)>,
) -> Result<(), HarnessError> {
    let OperationRecord::ToolStarted {
        assistant_entry_id,
        tool_index,
        call,
        result_entry_id,
        ..
    } = record
    else {
        return Ok(());
    };
    if !invocations.insert((assistant_entry_id.clone(), *tool_index)) {
        return Err(corrupt_recovery(
            RecoveryCorruptionReason::DuplicateToolInvocation,
            format!("tool invocation {assistant_entry_id}:{tool_index} is duplicated"),
        ));
    }
    let Some(SessionEntry::Message {
        message: AgentRecord::Llm(Message::Assistant(assistant)),
        ..
    }) = state.entry(assistant_entry_id)
    else {
        return Err(corrupt_recovery(
            RecoveryCorruptionReason::ToolCallMismatch,
            format!(
                "tool start {} does not reference an assistant entry",
                record.base().id
            ),
        ));
    };
    let source_call = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call),
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. } => None,
        })
        .nth(*tool_index as usize);
    if source_call
        .is_none_or(|source_call| source_call.id != call.id || source_call.name != call.name)
    {
        return Err(corrupt_recovery(
            RecoveryCorruptionReason::ToolCallMismatch,
            format!(
                "tool start {} does not match its assistant tool-call ordinal",
                record.base().id
            ),
        ));
    }
    validate_result_entry(
        state,
        result_entry_id,
        |entry| {
            matches!(
                entry,
                SessionEntry::Message {
                    message: AgentRecord::Llm(Message::ToolResult(result)),
                    ..
                } if result.tool_call_id == call.id && result.tool_name == call.name
            )
        },
        "tool result",
    )
}

fn validate_deferred_handles(state: &SessionState) -> Result<(), HarnessError> {
    for entry in state.entries_in_sequence_order() {
        if matches!(
            entry,
            SessionEntry::Message {
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } if assistant.finish.reason == AssistantFinishReason::Deferred
                && assistant.deferred.is_none()
        ) {
            return Err(corrupt_recovery(
                RecoveryCorruptionReason::InvalidDeferredHandle,
                format!("deferred assistant entry {} has no handle", entry.id()),
            ));
        }
    }
    Ok(())
}

/// Validates the bounded durable prefix consumed by operation recovery.
///
/// `pi-agent-session` deliberately accepts several imported contradictions so
/// it can expose multiple open starts. The live harness is the writer and
/// recovery boundary, so it applies pinned Pi's stricter reducer validation
/// before restoring queues or invoking any model/tool capability.
fn validate_recovery_log(
    state: &SessionState,
    lane: &agentprism_session::LaneName,
) -> Result<(), HarnessError> {
    validate_deferred_handles(state)?;
    let mut starts = BTreeMap::<RunId, agentprism_session::Sequence>::new();
    let mut finished = BTreeMap::<RunId, agentprism_session::Sequence>::new();
    let mut aborted = BTreeMap::<RunId, agentprism_session::Sequence>::new();
    let mut attempts = BTreeMap::<RunId, RecoveryAttempt>::new();
    let mut enqueued = BTreeMap::<(Option<RunId>, EntryId), agentprism_session::Sequence>::new();
    let mut tool_invocations = BTreeSet::new();

    for record in state
        .records_in_sequence_order()
        .iter()
        .filter(|record| record.lane() == lane)
    {
        if let OperationRecord::Started { intent, .. } = record {
            let run_id = record.run_id().ok_or(HarnessError::InvalidRecoveryRecord)?;
            starts.insert(run_id, record.sequence());
            validate_operation_result(state, intent)?;
            continue;
        }

        if let Some(run_id) = record.run_id() {
            if !starts.contains_key(&run_id) {
                return Err(corrupt_recovery(
                    RecoveryCorruptionReason::UnknownOperation,
                    format!(
                        "record {} references unknown operation {run_id}",
                        record.base().id
                    ),
                ));
            }
            if finished
                .get(&run_id)
                .is_some_and(|sequence| record.sequence() > *sequence)
            {
                return Err(corrupt_recovery(
                    RecoveryCorruptionReason::RecordAfterFinish,
                    format!(
                        "record {} follows operation {run_id}'s finish",
                        record.base().id
                    ),
                ));
            }
        }

        match record {
            OperationRecord::Finished { run_id, .. } => {
                finished.insert(run_id.clone(), record.sequence());
            }
            OperationRecord::AbortRequested { run_id, .. } => {
                aborted.insert(run_id.clone(), record.sequence());
            }
            OperationRecord::StepAttempt {
                run_id,
                step,
                attempt,
                result_entry_id,
                compaction_reason,
                ..
            } => {
                let reason_is_valid = match step {
                    OperationStep::Compaction => compaction_reason.is_some(),
                    OperationStep::Assistant | OperationStep::BranchSummary => {
                        compaction_reason.is_none()
                    }
                };
                if !reason_is_valid {
                    return Err(corrupt_recovery(
                        RecoveryCorruptionReason::InvalidCompactionReason,
                        format!(
                            "step attempt {} carries an invalid compaction reason",
                            record.base().id
                        ),
                    ));
                }

                let previous = attempts.get(run_id);
                let continues = previous.is_some_and(|previous| {
                    previous.step == *step
                        && state
                            .entry(&previous.result_entry_id)
                            .is_none_or(|entry| entry.sequence() >= record.sequence())
                });
                let expected = if continues {
                    previous
                        .map(|previous| previous.attempt.saturating_add(1))
                        .unwrap_or(1)
                } else {
                    1
                };
                if *attempt != expected {
                    return Err(corrupt_recovery(
                        RecoveryCorruptionReason::NonConsecutiveAttempt,
                        format!(
                            "{step:?} attempt {} is {attempt}; expected {expected}",
                            record.base().id
                        ),
                    ));
                }
                if continues
                    && *step != OperationStep::Assistant
                    && previous.is_some_and(|previous| {
                        previous.result_entry_id != *result_entry_id
                            || previous.compaction_reason != *compaction_reason
                    })
                {
                    return Err(corrupt_recovery(
                        RecoveryCorruptionReason::InconsistentStep,
                        format!(
                            "structural attempt {} changed its stable intent",
                            record.base().id
                        ),
                    ));
                }
                validate_attempt_result(state, *step, result_entry_id)?;
                attempts.insert(
                    run_id.clone(),
                    RecoveryAttempt {
                        step: *step,
                        attempt: *attempt,
                        result_entry_id: result_entry_id.clone(),
                        compaction_reason: *compaction_reason,
                    },
                );
            }
            OperationRecord::QueueEnqueued {
                run_id,
                queue,
                target,
                ..
            } => {
                if *queue != QueueKind::NextRun
                    && run_id.as_ref().is_some_and(|run_id| {
                        aborted
                            .get(run_id)
                            .is_some_and(|sequence| record.sequence() > *sequence)
                    })
                {
                    return Err(corrupt_recovery(
                        RecoveryCorruptionReason::QueueAfterAbort,
                        format!("{queue:?} item {} was enqueued after abort", target.id()),
                    ));
                }
                enqueued.insert((run_id.clone(), target.id().clone()), record.sequence());
                validate_exact_provisioned_entry(state, target)?;
            }
            OperationRecord::QueueCancelled {
                run_id, entry_id, ..
            } => {
                let key = (run_id.clone(), entry_id.clone());
                if enqueued
                    .get(&key)
                    .is_none_or(|sequence| *sequence >= record.sequence())
                    || state.entry(entry_id).is_some()
                {
                    return Err(corrupt_recovery(
                        RecoveryCorruptionReason::InvalidQueueCancellation,
                        format!(
                            "queue cancellation {} has no pending matching enqueue",
                            record.base().id
                        ),
                    ));
                }
            }
            OperationRecord::ToolStarted { .. } => {
                validate_tool_start(state, record, &mut tool_invocations)?;
            }
            OperationRecord::Started { .. } | OperationRecord::Usage { .. } => {}
            OperationRecord::WriteDeferred { target, .. } => {
                validate_exact_provisioned_entry(state, target)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct RecoveredToolStart {
    effective_args: serde_json::Value,
    result_entry_id: EntryId,
    replay: ToolReplayPolicy,
}

fn configured_tool_replay_policy(
    policies: &BTreeMap<String, ToolReplayPolicy>,
    tool_name: &str,
) -> ToolReplayPolicy {
    policies
        .get(tool_name)
        .copied()
        .unwrap_or(ToolReplayPolicy::Never)
}

#[derive(Clone)]
struct RecoveredToolCall {
    source_index: usize,
    call: ToolCall,
    started: Option<RecoveredToolStart>,
}

#[derive(Clone)]
struct RecoveredToolBatch {
    assistant_entry_id: EntryId,
    assistant: AssistantMessage,
    unresolved: Vec<RecoveredToolCall>,
    existing_results: BTreeMap<usize, (ToolResultMessage, bool)>,
    new_messages: Vec<AgentRecord>,
    run_state: RecoveredRunState,
}

enum RunRecoveryPlan {
    CommitMissing {
        missing: Vec<ProvisionedEntry>,
        run_state: RecoveredRunState,
    },
    Continue(RecoveredRunState),
    RecoverTools(Box<RecoveredToolBatch>),
    ResumeCompleted(Box<RecoveredCompletedTurn>),
}

fn run_resume_intent(operation: &OperationRecord) -> Result<RunResumeIntent, HarnessError> {
    let OperationRecord::Started { intent, .. } = operation else {
        return Err(HarnessError::InvalidRecoveryRecord);
    };
    let OperationIntent::Run {
        original_prompt,
        initial_messages,
        system_prompt_override,
        resume_data,
    } = intent
    else {
        return Err(HarnessError::UnsupportedRecovery {
            operation: operation_intent_name(intent).to_owned(),
        });
    };
    Ok(RunResumeIntent {
        run_id: operation
            .run_id()
            .ok_or(HarnessError::InvalidRecoveryRecord)?,
        original_prompt: original_prompt.clone(),
        initial_messages: initial_messages.clone(),
        system_prompt_override: system_prompt_override.clone(),
        resume_data: resume_data.clone(),
    })
}

fn reconstruct_run_state(
    records: &[OperationRecord],
    run_id: &RunId,
    system_prompt_override: Option<String>,
    new_messages: Vec<AgentRecord>,
) -> RecoveredRunState {
    let mut usage = None;
    let mut cost = None;
    let mut cost_complete = true;
    for record in records {
        let OperationRecord::Usage {
            attribution,
            usage: next_usage,
            cost: next_cost,
            ..
        } = record
        else {
            continue;
        };
        let belongs_to_run = matches!(
            attribution,
            agentprism_session::UsageAttribution::Assistant {
                run_id: owner, ..
            } | agentprism_session::UsageAttribution::DeferredFetch {
                run_id: owner, ..
            } if owner == run_id
        );
        if !belongs_to_run {
            continue;
        }
        usage = Some(match usage.take() {
            None => next_usage.clone(),
            Some(total) => add_recovered_usage(total, next_usage),
        });
        if cost_complete {
            match add_recovered_cost(cost.take(), next_cost.as_ref()) {
                Ok(total) => cost = total,
                Err(()) => cost_complete = false,
            }
        }
    }
    RecoveredRunState {
        usage,
        cost,
        cost_complete,
        system_prompt_override,
        new_messages,
    }
}

fn add_recovered_cost(total: Option<Cost>, next: Option<&Cost>) -> Result<Option<Cost>, ()> {
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

fn add_recovered_usage(mut total: Usage, next: &Usage) -> Usage {
    let total_tokens = if total.total_tokens.is_some() || next.total_tokens.is_some() {
        let combined = total.total_tokens().saturating_add(next.total_tokens());
        Some(u64::try_from(combined).unwrap_or(u64::MAX))
    } else {
        None
    };
    total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
    total.reasoning_tokens = add_recovered_optional(total.reasoning_tokens, next.reasoning_tokens);
    total.cache_read_tokens =
        add_recovered_optional(total.cache_read_tokens, next.cache_read_tokens);
    total.cache_write_tokens =
        add_recovered_optional(total.cache_write_tokens, next.cache_write_tokens);
    total.cache_write_one_hour_tokens = add_recovered_optional(
        total.cache_write_one_hour_tokens,
        next.cache_write_one_hour_tokens,
    );
    total.total_tokens = total_tokens;
    if total.source != next.source {
        total.source = UsageSource::Mixed;
    }
    total
}

fn add_recovered_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

fn derive_run_recovery_plan(
    state: &SessionState,
    lane: &agentprism_session::LaneName,
    operation: &OperationRecord,
    completed_steps: &[OperationRecord],
) -> Result<RunRecoveryPlan, HarnessError> {
    let OperationRecord::Started { base, intent, .. } = operation else {
        return Err(HarnessError::InvalidRecoveryRecord);
    };
    let OperationIntent::Run {
        initial_messages,
        system_prompt_override,
        ..
    } = intent
    else {
        return Err(HarnessError::UnsupportedRecovery {
            operation: operation_intent_name(intent).to_owned(),
        });
    };

    let own_entries = state
        .lane_leaf(lane)
        .and_then(Option::as_ref)
        .map(|leaf| state.scan_branch_root_to_leaf(leaf))
        .transpose()
        .map_err(agentprism_session::SessionError::from)?
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.sequence() > base.sequence)
        .collect::<Vec<_>>();
    let newest = own_entries.last().copied();
    let new_messages = own_entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { message, .. } => Some(message.clone()),
            SessionEntry::ModelChange { .. }
            | SessionEntry::ReasoningChange { .. }
            | SessionEntry::ActiveToolsChange { .. }
            | SessionEntry::Compaction { .. }
            | SessionEntry::BranchSummary { .. }
            | SessionEntry::Custom { .. } => None,
        })
        .collect::<Vec<_>>();
    let run_id = operation
        .run_id()
        .ok_or(HarnessError::InvalidRecoveryRecord)?;
    let run_state = reconstruct_run_state(
        completed_steps,
        &run_id,
        system_prompt_override.clone(),
        new_messages.clone(),
    );

    let missing = initial_messages
        .iter()
        .filter(|target| state.entry(target.id()).is_none())
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Ok(RunRecoveryPlan::CommitMissing { missing, run_state });
    }

    if let Some(OperationRecord::StepAttempt {
        step,
        result_entry_id,
        ..
    }) = completed_steps
        .iter()
        .rev()
        .find(|record| matches!(record, OperationRecord::StepAttempt { .. }))
        && state.entry(result_entry_id).is_none()
    {
        return match step {
            OperationStep::Assistant => Ok(RunRecoveryPlan::Continue(run_state)),
            OperationStep::Compaction | OperationStep::BranchSummary => {
                Err(HarnessError::UnsupportedRecovery {
                    operation: format!("unfinished_{step:?}").to_lowercase(),
                })
            }
        };
    }

    if let Some(SessionEntry::Message {
        message: AgentRecord::Llm(Message::Assistant(assistant)),
        ..
    }) = newest
        && matches!(
            assistant.finish.reason,
            AssistantFinishReason::Error | AssistantFinishReason::Aborted
        )
    {
        let produced_by_step = completed_steps.iter().any(|record| {
            matches!(
                record,
                OperationRecord::StepAttempt { result_entry_id, .. }
                    if result_entry_id == newest.expect("matched newest entry").id()
            )
        });
        let produced_by_deferred_fetch = completed_steps.iter().any(|record| {
            matches!(
                record,
                OperationRecord::Usage {
                    attribution: agentprism_session::UsageAttribution::DeferredFetch { entry_id, .. },
                    ..
                } if entry_id == newest.expect("matched newest entry").id()
            )
        });
        if produced_by_step || produced_by_deferred_fetch {
            return Ok(RunRecoveryPlan::ResumeCompleted(Box::new(
                RecoveredCompletedTurn {
                    assistant: assistant.clone(),
                    tool_results: Vec::new(),
                    terminate_batch: false,
                    new_messages,
                    run_state,
                },
            )));
        }
    }

    // Pinned Pi's reducer makes the exact newest operation-owned entry
    // authoritative (`newestOwn`). A preceding assistant cannot close the run
    // after a later steering/follow-up/custom entry was durably consumed but
    // before its next assistant attempt began.
    let Some((assistant_entry, assistant)) = newest.and_then(|entry| match entry {
        SessionEntry::Message {
            message: AgentRecord::Llm(Message::Assistant(assistant)),
            ..
        } => Some((entry, assistant)),
        SessionEntry::Message { .. }
        | SessionEntry::ModelChange { .. }
        | SessionEntry::ReasoningChange { .. }
        | SessionEntry::ActiveToolsChange { .. }
        | SessionEntry::Compaction { .. }
        | SessionEntry::BranchSummary { .. }
        | SessionEntry::Custom { .. } => None,
    }) else {
        return Ok(RunRecoveryPlan::Continue(run_state));
    };
    if assistant.finish.reason == AssistantFinishReason::Deferred {
        return Err(HarnessError::UnsupportedRecovery {
            operation: "deferred_assistant".to_owned(),
        });
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
    if tool_calls.is_empty() {
        return Ok(RunRecoveryPlan::ResumeCompleted(Box::new(
            RecoveredCompletedTurn {
                assistant: assistant.clone(),
                tool_results: Vec::new(),
                terminate_batch: false,
                new_messages,
                run_state,
            },
        )));
    }

    let deferred_write_ids = completed_steps
        .iter()
        .filter_map(|record| match record {
            OperationRecord::WriteDeferred { target, .. } => Some(target.id().clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let starts = completed_steps
        .iter()
        .filter_map(|record| match record {
            OperationRecord::ToolStarted {
                assistant_entry_id,
                tool_index,
                effective_args,
                result_entry_id,
                replay,
                ..
            } if assistant_entry_id == assistant_entry.id() => Some((
                *tool_index,
                RecoveredToolStart {
                    effective_args: effective_args.clone(),
                    result_entry_id: result_entry_id.clone(),
                    replay: *replay,
                },
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut unresolved = Vec::new();
    let mut existing_results = BTreeMap::new();
    for (source_index, call) in tool_calls.into_iter().enumerate() {
        let started = u32::try_from(source_index)
            .ok()
            .and_then(|index| starts.get(&index).cloned());
        let started_result = started
            .as_ref()
            .and_then(|started| state.entry(&started.result_entry_id));
        let blocked_result = own_entries.iter().copied().find(|entry| {
            entry.sequence() > assistant_entry.sequence()
                && !deferred_write_ids.contains(entry.id())
                && matches!(
                    entry,
                    SessionEntry::Message {
                        message: AgentRecord::Llm(Message::ToolResult(result)),
                        ..
                    } if result.tool_call_id == call.id
                )
        });
        let result = started_result.or(blocked_result);
        if let Some(SessionEntry::Message {
            message: AgentRecord::Llm(Message::ToolResult(result)),
            terminate,
            ..
        }) = result
        {
            existing_results.insert(source_index, (result.clone(), *terminate));
        } else {
            unresolved.push(RecoveredToolCall {
                source_index,
                call,
                started,
            });
        }
    }
    if !unresolved.is_empty() {
        return Ok(RunRecoveryPlan::RecoverTools(Box::new(
            RecoveredToolBatch {
                assistant_entry_id: assistant_entry.id().clone(),
                assistant: assistant.clone(),
                unresolved,
                existing_results,
                new_messages,
                run_state,
            },
        )));
    }
    let terminate_batch =
        !existing_results.is_empty() && existing_results.values().all(|(_, terminate)| *terminate);
    Ok(RunRecoveryPlan::ResumeCompleted(Box::new(
        RecoveredCompletedTurn {
            assistant: assistant.clone(),
            tool_results: existing_results
                .into_values()
                .map(|(result, _)| result)
                .collect(),
            terminate_batch,
            new_messages,
            run_state,
        },
    )))
}

fn recover_send_tool_batch(
    agent: &Agent,
    session: &Session,
    run_id: &RunId,
    batch: RecoveredToolBatch,
    tool_replay_policies: &BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Arc<SendDurableToolState>,
    cancellation: CancellationToken,
) -> Result<CompletedTurnRecoveryStream<'static>, HarnessError> {
    let scheduler = agent.tool_scheduler();
    let tools = agent.tools().clone();
    let context = AgentContext {
        system_prompt: batch
            .run_state
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| agent.state().system_prompt.clone()),
        records: agent.state().transcript.clone(),
        tools: tools.clone(),
    };
    let run_state = batch.run_state.clone();
    let configured_mode =
        recovered_execution_mode(agent.tool_execution_mode(), &batch.assistant, |name| {
            tools.get(name).map(|tool| tool.execution_mode())
        });
    let sequential_lifecycle = configured_mode == ToolExecutionMode::Sequential
        || batch.assistant.finish.reason == AssistantFinishReason::Length;
    let mut execution_calls = Vec::new();
    let mut recovered_arguments = BTreeMap::<ToolCallId, serde_json::Value>::new();
    let mut targets = BTreeMap::<ToolCallId, (usize, EntryId)>::new();
    let mut durable_calls = BTreeMap::<ToolCallId, DurableToolStartIntent>::new();
    let mut completed = BTreeMap::<ToolCallId, (ToolOutput, bool)>::new();

    for pending in &batch.unresolved {
        let (call, result_entry_id, execute, durable_state, replay) = match &pending.started {
            Some(started) if started.replay == ToolReplayPolicy::Never => (
                pending.call.clone(),
                started.result_entry_id.clone(),
                false,
                DurableToolStartState::Persisted,
                started.replay,
            ),
            Some(started) => {
                recovered_arguments.insert(pending.call.id.clone(), started.effective_args.clone());
                (
                    pending.call.clone(),
                    started.result_entry_id.clone(),
                    true,
                    DurableToolStartState::Persisted,
                    started.replay,
                )
            }
            None => {
                let result_entry_id = session.next_entry_id("tool-result");
                (
                    pending.call.clone(),
                    result_entry_id,
                    true,
                    DurableToolStartState::Pending,
                    configured_tool_replay_policy(tool_replay_policies, &pending.call.name),
                )
            }
        };
        targets.insert(
            pending.call.id.clone(),
            (pending.source_index, result_entry_id.clone()),
        );
        if execute {
            durable_calls.insert(
                call.id.clone(),
                DurableToolStartIntent {
                    tool_index: u32::try_from(pending.source_index)
                        .map_err(|_| HarnessError::InvalidRecoveryRecord)?,
                    call: ToolCallIdentity {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    },
                    result_entry_id: result_entry_id.clone(),
                    replay,
                    state: durable_state,
                },
            );
            execution_calls.push(call);
        } else {
            completed.insert(
                pending.call.id.clone(),
                interrupted_tool_output(&pending.call),
            );
        }
    }
    tool_durability.install(DurableToolPlan {
        run_id: run_id.clone(),
        assistant_message_id: batch.assistant.id.clone(),
        assistant_entry_id: batch.assistant_entry_id.clone(),
        calls: durable_calls,
    });

    let mut ordered = batch.unresolved.clone();
    ordered.sort_by_key(|call| call.source_index);
    Ok(Box::pin(async_stream::stream! {
        let mut next_commit = 0;
        let mut all_results = batch.existing_results.clone();
        let mut new_messages = batch.new_messages.clone();
        if sequential_lifecycle {
            match take_ready_recovered_results(
                &batch,
                &ordered,
                &targets,
                &mut completed,
                &mut next_commit,
                &mut all_results,
                &mut new_messages,
            ) {
                Ok(events) => {
                    for event in events {
                        yield event;
                    }
                }
                Err(error) => {
                    yield CompletedTurnRecoveryEvent::Failed(recovery_public_error(error));
                    return;
                }
            }
        }
        if !execution_calls.is_empty() {
            let mut stream = scheduler.execute_recovery_batch_events(
                &tools,
                ToolBatchRequest {
                    assistant: &batch.assistant,
                    calls: &execution_calls,
                    context: &context,
                    configured_mode,
                    cancellation,
                },
                &recovered_arguments,
            );
            while let Some(event) = stream.next().await {
                match event {
                    ToolBatchStreamEvent::BatchStarted { .. }
                    | ToolBatchStreamEvent::BatchFinished { .. } => {}
                    ToolBatchStreamEvent::CallStarted { call, .. } => {
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionStarted { call }),
                        );
                    }
                    ToolBatchStreamEvent::CallUpdated {
                        call_id, update, ..
                    } => {
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionUpdated { call_id, update }),
                        );
                    }
                    ToolBatchStreamEvent::CallFinished { outcome } => {
                        let outcome = *outcome;
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionFinished {
                                call_id: outcome.call.id.clone(),
                                result: outcome.output.clone(),
                                is_error: outcome.is_error,
                            }),
                        );
                        completed.insert(outcome.call.id, (outcome.output, outcome.is_error));
                        if sequential_lifecycle {
                            match take_ready_recovered_results(
                                &batch,
                                &ordered,
                                &targets,
                                &mut completed,
                                &mut next_commit,
                                &mut all_results,
                                &mut new_messages,
                            ) {
                                Ok(events) => {
                                    for event in events {
                                        yield event;
                                    }
                                }
                                Err(error) => {
                                    yield CompletedTurnRecoveryEvent::Failed(
                                        recovery_public_error(error),
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
        for pending in ordered.iter().skip(next_commit) {
            completed
                .entry(pending.call.id.clone())
                .or_insert_with(|| interrupted_tool_output(&pending.call));
        }
        match take_ready_recovered_results(
            &batch,
            &ordered,
            &targets,
            &mut completed,
            &mut next_commit,
            &mut all_results,
            &mut new_messages,
        ) {
            Ok(events) => {
                for event in events {
                    yield event;
                }
            }
            Err(error) => {
                yield CompletedTurnRecoveryEvent::Failed(recovery_public_error(error));
                return;
            }
        }
        let terminate_batch = !all_results.is_empty()
            && all_results.values().all(|(_, terminate)| *terminate);
        yield CompletedTurnRecoveryEvent::Completed(Box::new(RecoveredCompletedTurn {
            assistant: batch.assistant,
            tool_results: all_results
                .into_values()
                .map(|(result, _)| result)
                .collect(),
            terminate_batch,
            new_messages,
            run_state,
        }));
    }))
}

fn recover_local_tool_batch(
    agent: &LocalAgent,
    session: &LocalSession,
    run_id: &RunId,
    batch: RecoveredToolBatch,
    tool_replay_policies: &BTreeMap<String, ToolReplayPolicy>,
    tool_durability: Rc<LocalDurableToolState>,
    cancellation: CancellationToken,
) -> Result<LocalCompletedTurnRecoveryStream<'static>, HarnessError> {
    let scheduler = agent.tool_scheduler();
    let tools = agent.tools().clone();
    let context = LocalAgentContext {
        system_prompt: batch
            .run_state
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| agent.state().system_prompt.clone()),
        records: agent.state().transcript.clone(),
        tools: tools.clone(),
    };
    let run_state = batch.run_state.clone();
    let configured_mode =
        recovered_execution_mode(agent.tool_execution_mode(), &batch.assistant, |name| {
            tools.get(name).map(|tool| tool.execution_mode())
        });
    let sequential_lifecycle = configured_mode == ToolExecutionMode::Sequential
        || batch.assistant.finish.reason == AssistantFinishReason::Length;
    let mut execution_calls = Vec::new();
    let mut recovered_arguments = BTreeMap::<ToolCallId, serde_json::Value>::new();
    let mut targets = BTreeMap::<ToolCallId, (usize, EntryId)>::new();
    let mut durable_calls = BTreeMap::<ToolCallId, DurableToolStartIntent>::new();
    let mut completed = BTreeMap::<ToolCallId, (ToolOutput, bool)>::new();

    for pending in &batch.unresolved {
        let (call, result_entry_id, execute, durable_state, replay) = match &pending.started {
            Some(started) if started.replay == ToolReplayPolicy::Never => (
                pending.call.clone(),
                started.result_entry_id.clone(),
                false,
                DurableToolStartState::Persisted,
                started.replay,
            ),
            Some(started) => {
                recovered_arguments.insert(pending.call.id.clone(), started.effective_args.clone());
                (
                    pending.call.clone(),
                    started.result_entry_id.clone(),
                    true,
                    DurableToolStartState::Persisted,
                    started.replay,
                )
            }
            None => {
                let result_entry_id = session.next_entry_id("tool-result");
                (
                    pending.call.clone(),
                    result_entry_id,
                    true,
                    DurableToolStartState::Pending,
                    configured_tool_replay_policy(tool_replay_policies, &pending.call.name),
                )
            }
        };
        targets.insert(
            pending.call.id.clone(),
            (pending.source_index, result_entry_id.clone()),
        );
        if execute {
            durable_calls.insert(
                call.id.clone(),
                DurableToolStartIntent {
                    tool_index: u32::try_from(pending.source_index)
                        .map_err(|_| HarnessError::InvalidRecoveryRecord)?,
                    call: ToolCallIdentity {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    },
                    result_entry_id: result_entry_id.clone(),
                    replay,
                    state: durable_state,
                },
            );
            execution_calls.push(call);
        } else {
            completed.insert(
                pending.call.id.clone(),
                interrupted_tool_output(&pending.call),
            );
        }
    }
    tool_durability.install(DurableToolPlan {
        run_id: run_id.clone(),
        assistant_message_id: batch.assistant.id.clone(),
        assistant_entry_id: batch.assistant_entry_id.clone(),
        calls: durable_calls,
    });

    let mut ordered = batch.unresolved.clone();
    ordered.sort_by_key(|call| call.source_index);
    Ok(Box::pin(async_stream::stream! {
        let mut next_commit = 0;
        let mut all_results = batch.existing_results.clone();
        let mut new_messages = batch.new_messages.clone();
        if sequential_lifecycle {
            match take_ready_recovered_results(
                &batch,
                &ordered,
                &targets,
                &mut completed,
                &mut next_commit,
                &mut all_results,
                &mut new_messages,
            ) {
                Ok(events) => {
                    for event in events {
                        yield event;
                    }
                }
                Err(error) => {
                    yield CompletedTurnRecoveryEvent::Failed(recovery_public_error(error));
                    return;
                }
            }
        }
        if !execution_calls.is_empty() {
            let mut stream = scheduler.execute_recovery_batch_events(
                &tools,
                ToolBatchRequest {
                    assistant: &batch.assistant,
                    calls: &execution_calls,
                    context: &context,
                    configured_mode,
                    cancellation,
                },
                &recovered_arguments,
            );
            while let Some(event) = stream.next().await {
                match event {
                    ToolBatchStreamEvent::BatchStarted { .. }
                    | ToolBatchStreamEvent::BatchFinished { .. } => {}
                    ToolBatchStreamEvent::CallStarted { call, .. } => {
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionStarted { call }),
                        );
                    }
                    ToolBatchStreamEvent::CallUpdated {
                        call_id, update, ..
                    } => {
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionUpdated { call_id, update }),
                        );
                    }
                    ToolBatchStreamEvent::CallFinished { outcome } => {
                        let outcome = *outcome;
                        yield CompletedTurnRecoveryEvent::Agent(
                            Box::new(AgentEvent::ToolExecutionFinished {
                                call_id: outcome.call.id.clone(),
                                result: outcome.output.clone(),
                                is_error: outcome.is_error,
                            }),
                        );
                        completed.insert(outcome.call.id, (outcome.output, outcome.is_error));
                        if sequential_lifecycle {
                            match take_ready_recovered_results(
                                &batch,
                                &ordered,
                                &targets,
                                &mut completed,
                                &mut next_commit,
                                &mut all_results,
                                &mut new_messages,
                            ) {
                                Ok(events) => {
                                    for event in events {
                                        yield event;
                                    }
                                }
                                Err(error) => {
                                    yield CompletedTurnRecoveryEvent::Failed(
                                        recovery_public_error(error),
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
        for pending in ordered.iter().skip(next_commit) {
            completed
                .entry(pending.call.id.clone())
                .or_insert_with(|| interrupted_tool_output(&pending.call));
        }
        match take_ready_recovered_results(
            &batch,
            &ordered,
            &targets,
            &mut completed,
            &mut next_commit,
            &mut all_results,
            &mut new_messages,
        ) {
            Ok(events) => {
                for event in events {
                    yield event;
                }
            }
            Err(error) => {
                yield CompletedTurnRecoveryEvent::Failed(recovery_public_error(error));
                return;
            }
        }
        let terminate_batch = !all_results.is_empty()
            && all_results.values().all(|(_, terminate)| *terminate);
        yield CompletedTurnRecoveryEvent::Completed(Box::new(RecoveredCompletedTurn {
            assistant: batch.assistant,
            tool_results: all_results
                .into_values()
                .map(|(result, _)| result)
                .collect(),
            terminate_batch,
            new_messages,
            run_state,
        }));
    }))
}

fn recovered_execution_mode(
    configured: ToolExecutionMode,
    assistant: &AssistantMessage,
    mut lookup: impl FnMut(&str) -> Option<ToolExecutionMode>,
) -> ToolExecutionMode {
    if configured == ToolExecutionMode::Sequential
        || assistant.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolCall { call, .. }
                    if lookup(&call.name) == Some(ToolExecutionMode::Sequential)
            )
        })
    {
        ToolExecutionMode::Sequential
    } else {
        ToolExecutionMode::Parallel
    }
}

fn interrupted_tool_output(call: &ToolCall) -> (ToolOutput, bool) {
    (
        ToolOutput::new(vec![ToolResultContent::Text {
            id: ContentBlockId::new(format!("{}-recovery-error", call.id.as_str())),
            text: "Tool execution was interrupted before its result was committed; the tool is not replay-safe and was not run again."
                .to_owned(),
        }]),
        true,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovered lifecycle state is kept explicit at each durable boundary"
)]
fn take_ready_recovered_results(
    batch: &RecoveredToolBatch,
    ordered: &[RecoveredToolCall],
    targets: &BTreeMap<ToolCallId, (usize, EntryId)>,
    completed: &mut BTreeMap<ToolCallId, (ToolOutput, bool)>,
    next_commit: &mut usize,
    all_results: &mut BTreeMap<usize, (ToolResultMessage, bool)>,
    new_messages: &mut Vec<AgentRecord>,
) -> Result<Vec<CompletedTurnRecoveryEvent>, HarnessError> {
    let mut events = Vec::new();
    while let Some(pending) = ordered.get(*next_commit) {
        let Some((output, is_error)) = completed.remove(&pending.call.id) else {
            break;
        };
        let (source_index, _) = targets
            .get(&pending.call.id)
            .ok_or(HarnessError::InvalidRecoveryRecord)?;
        if *source_index != pending.source_index {
            return Err(HarnessError::InvalidRecoveryRecord);
        }
        let message = recovered_tool_result_message(
            &batch.assistant,
            pending.source_index,
            &pending.call,
            &output,
            is_error,
        );
        let record = AgentRecord::Llm(Message::ToolResult(message.clone()));
        all_results.insert(pending.source_index, (message.clone(), output.terminate));
        new_messages.push(record.clone());
        events.push(CompletedTurnRecoveryEvent::Agent(Box::new(
            AgentEvent::MessageStarted {
                message_id: message.id,
                role: MessageRole::ToolResult,
            },
        )));
        events.push(CompletedTurnRecoveryEvent::Agent(Box::new(
            AgentEvent::MessageCommitted { message: record },
        )));
        *next_commit += 1;
    }
    Ok(events)
}

fn recovery_public_error(error: HarnessError) -> PublicError {
    PublicError {
        code: "completed_turn_recovery".to_owned(),
        message: error.to_string(),
        retryable: false,
        provider_code: None,
        status: None,
        request_id: None,
    }
}

fn recovered_tool_result_message(
    assistant: &AssistantMessage,
    source_index: usize,
    call: &ToolCall,
    output: &ToolOutput,
    is_error: bool,
) -> ToolResultMessage {
    ToolResultMessage {
        id: MessageId::new(format!(
            "{}-tool-result-{source_index}",
            assistant.id.as_str()
        )),
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: output.content.clone(),
        details: output.details.clone().map(|value| VersionedExtension {
            schema_version: 1,
            value,
        }),
        usage: output.usage.clone(),
        added_tool_names: output.added_tool_names.clone(),
        is_error,
        timestamp: assistant.timestamp,
    }
}

fn provision_record(
    session: &Session,
    kind: &'static str,
    message: AgentRecord,
) -> ProvisionedEntry {
    ProvisionedEntry::Message {
        id: session.next_entry_id(kind),
        message,
        terminate: false,
    }
}

fn provision_local_record(
    session: &LocalSession,
    kind: &'static str,
    message: AgentRecord,
) -> ProvisionedEntry {
    ProvisionedEntry::Message {
        id: session.next_entry_id(kind),
        message,
        terminate: false,
    }
}

fn provisioned_record(target: &ProvisionedEntry) -> Option<&AgentRecord> {
    match target {
        ProvisionedEntry::Message { message, .. } => Some(message),
        ProvisionedEntry::ModelChange { .. }
        | ProvisionedEntry::ReasoningChange { .. }
        | ProvisionedEntry::ActiveToolsChange { .. }
        | ProvisionedEntry::Compaction { .. }
        | ProvisionedEntry::BranchSummary { .. }
        | ProvisionedEntry::Custom { .. } => None,
    }
}

fn empty_run_intent() -> OperationIntent {
    OperationIntent::Run {
        original_prompt: Vec::new(),
        initial_messages: Vec::new(),
        system_prompt_override: None,
        resume_data: BTreeMap::new(),
    }
}

fn operation_intent_name(intent: &OperationIntent) -> &'static str {
    match intent {
        OperationIntent::Run { .. } => "run",
        OperationIntent::Compaction { .. } => "compaction",
        OperationIntent::Navigation { .. } => "navigation",
    }
}

fn operation_terminal(
    outcome: &AgentRunOutcome,
) -> (OperationOutcome, HarnessRunOutcome, Option<PublicError>) {
    match outcome {
        AgentRunOutcome::Completed { .. } => (
            OperationOutcome::Completed,
            HarnessRunOutcome::Completed,
            None,
        ),
        AgentRunOutcome::Failed { error, .. } => (
            OperationOutcome::Failed,
            HarnessRunOutcome::Failed,
            Some(error.clone()),
        ),
        AgentRunOutcome::Cancelled { reason, .. } => (
            OperationOutcome::Aborted,
            HarnessRunOutcome::Aborted,
            Some(cancel_public_error(reason)),
        ),
    }
}

fn cancel_public_error(reason: &CancellationReason) -> PublicError {
    PublicError {
        code: "cancelled".to_owned(),
        message: reason.message.clone(),
        retryable: false,
        provider_code: None,
        status: None,
        request_id: None,
    }
}

struct ActiveRunGuard {
    active: Arc<Mutex<Option<ActiveHarnessRun>>>,
    operation_run_id: RunId,
    finished: bool,
}

impl ActiveRunGuard {
    fn new(active: Arc<Mutex<Option<ActiveHarnessRun>>>, value: ActiveHarnessRun) -> Self {
        let operation_run_id = value.operation_run_id.clone();
        *lock_unpoisoned(&active) = Some(value);
        Self {
            active,
            operation_run_id,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        self.clear();
    }

    fn mark_closing(&self) {
        let mut active = lock_unpoisoned(&self.active);
        if let Some(value) = active.as_mut()
            && value.operation_run_id == self.operation_run_id
        {
            value.accepting_ingress = false;
        }
    }

    fn clear(&self) {
        let mut active = lock_unpoisoned(&self.active);
        if active
            .as_ref()
            .is_some_and(|value| value.operation_run_id == self.operation_run_id)
        {
            *active = None;
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

struct LocalActiveRunGuard {
    active: Rc<RefCell<Option<ActiveHarnessRun>>>,
    operation_run_id: RunId,
}

impl LocalActiveRunGuard {
    fn new(active: Rc<RefCell<Option<ActiveHarnessRun>>>, value: ActiveHarnessRun) -> Self {
        let operation_run_id = value.operation_run_id.clone();
        *active.borrow_mut() = Some(value);
        Self {
            active,
            operation_run_id,
        }
    }

    fn finish(&mut self) {
        self.clear();
    }

    fn mark_closing(&self) {
        if let Some(value) = self.active.borrow_mut().as_mut()
            && value.operation_run_id == self.operation_run_id
        {
            value.accepting_ingress = false;
        }
    }

    fn clear(&self) {
        let should_clear = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|value| value.operation_run_id == self.operation_run_id);
        if should_clear {
            *self.active.borrow_mut() = None;
        }
    }
}

impl Drop for LocalActiveRunGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
