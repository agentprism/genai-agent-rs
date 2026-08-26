use futures_channel::oneshot;
use futures_executor::block_on;
use futures_util::StreamExt;
use pi_agent_core::{
    AfterToolCall, Agent, AgentEvent, AgentRecord, AgentState, BeforeToolCall, CompletedTurn,
    LocalAfterToolCall, LocalAgent, LocalBeforeToolCall, LocalCompletedTurn, LocalNextTurn,
    LocalTool, LocalToolArgumentPreparer, LocalToolPolicy, LocalToolRegistry, LocalToolUpdateSink,
    LocalTurnPolicy, MessageRole, NextTurn, Tool, ToolArgumentPreparer, ToolAuthorization,
    ToolCallContext, ToolExecutionMode, ToolOutput, ToolOutputPatch, ToolPolicy, ToolRegistry,
    ToolUpdate, ToolUpdateSink, TurnPolicy, TurnPolicyError,
};
use pi_agent_env::{Clock, ClockError, LocalClock};
use pi_agent_harness::*;
use pi_agent_session::{
    AppendReceipt, EntryBase, EntryId, InMemorySessionStorage, LaneName, LocalSessionStorage,
    OperationIntent, OperationOutcome, OperationRecord, OperationRecordBase, OperationRecordId,
    OperationStep, ProvisionedEntry, QueueKind, RecoveryDecision, SESSION_METADATA_SCHEMA_VERSION,
    Sequence, SessionEntry, SessionEnvironmentMetadata, SessionError, SessionErrorKind,
    SessionHeader, SessionId, SessionMetadata, SessionMutation, SessionState, SessionStorage,
    TAIL_REPAIR_REPORT_SCHEMA_VERSION, TailRepairReport, ToolReplayPolicy,
};
use pi_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    CancellationToken, ContentBlock, ContentBlockId, ContentBlockKind, Cost, Currency,
    LocalBoxFuture, Message, MessageId, ModelId, ModelRef, ProviderId, PublicError, ReasoningLevel,
    RunId, ScriptedResponse, ScriptedRuntime, SendBoxFuture, Timestamp, ToolCallId,
    ToolResultContent, ToolSpec, Usage, UsageSource, UserMessage, VersionedExtension,
    text_response, tool_call_response,
};
use serde_json::{json, value::to_raw_value};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy)]
struct FixedClock(Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move { cancellation.check().map_err(|_| ClockError::Cancelled) })
    }
}

impl LocalClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move { cancellation.check().map_err(|_| ClockError::Cancelled) })
    }
}

fn agent_state() -> AgentState {
    AgentState::new(
        "You are helpful.",
        ModelRef::new("scripted", "test-model"),
        ReasoningLevel::Off,
    )
}

fn user(id: &str, text: &str) -> AgentRecord {
    AgentRecord::Llm(Message::User(UserMessage {
        id: MessageId::new(id),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-text")),
            text: text.to_owned(),
        }],
        timestamp: Timestamp::from_unix_millis(1),
    }))
}

fn storage(id: &str) -> Arc<InMemorySessionStorage> {
    Arc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            id,
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .expect("valid in-memory session"),
    )
}

fn session(storage: Arc<dyn SessionStorage>, prefix: &str) -> Arc<Session> {
    Arc::new(Session::new(
        storage,
        LaneName::new("main"),
        Arc::new(MonotonicHarnessIdGenerator::new(prefix)),
        Arc::new(FixedClock(Timestamp::from_unix_millis(10))),
    ))
}

fn local_session(storage: Rc<dyn LocalSessionStorage>, prefix: &str) -> Rc<LocalSession> {
    Rc::new(LocalSession::new(
        storage,
        LaneName::new("main"),
        Rc::new(MonotonicHarnessIdGenerator::new(prefix)),
        Rc::new(FixedClock(Timestamp::from_unix_millis(10))),
    ))
}

fn agent(responses: impl IntoIterator<Item = ScriptedResponse>, tools: ToolRegistry) -> Agent {
    Agent::new(
        Arc::new(ScriptedRuntime::new(responses)),
        agent_state(),
        tools,
    )
    .expect("valid scripted agent")
}

fn local_agent(responses: impl IntoIterator<Item = ScriptedResponse>) -> LocalAgent {
    LocalAgent::new(
        Rc::new(ScriptedRuntime::new(responses)),
        agent_state(),
        LocalToolRegistry::new(),
    )
    .expect("valid local scripted agent")
}

#[derive(Clone, Copy)]
enum StopQueue {
    Steering,
    FollowUp,
}

struct QueueDuringStopPolicy {
    control: Arc<Mutex<Option<HarnessControl>>>,
    queue: StopQueue,
    message: AgentRecord,
    enqueued: AtomicBool,
    receipts: Arc<Mutex<Vec<DurableQueueReceipt>>>,
}

impl TurnPolicy for QueueDuringStopPolicy {
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
        Box::pin(async move {
            if !self.enqueued.swap(true, Ordering::AcqRel) {
                let control = lock(&self.control)
                    .clone()
                    .expect("harness control installed before run");
                let receipt = match self.queue {
                    StopQueue::Steering => control.steer(self.message.clone()).await,
                    StopQueue::FollowUp => control.follow_up(self.message.clone()).await,
                }
                .map_err(|error| TurnPolicyError::new(error.to_string()))?;
                lock(&self.receipts).push(receipt);
            }
            Ok(true)
        })
    }
}

struct LocalQueueDuringStopPolicy {
    control: Rc<RefCell<Option<LocalHarnessControl>>>,
    queue: StopQueue,
    message: AgentRecord,
    enqueued: RefCell<bool>,
    receipts: Rc<RefCell<Vec<DurableQueueReceipt>>>,
}

impl LocalTurnPolicy for LocalQueueDuringStopPolicy {
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
        Box::pin(async move {
            if !*self.enqueued.borrow() {
                *self.enqueued.borrow_mut() = true;
                let control = self
                    .control
                    .borrow()
                    .clone()
                    .expect("local harness control installed before run");
                let receipt = match self.queue {
                    StopQueue::Steering => control.steer(self.message.clone()).await,
                    StopQueue::FollowUp => control.follow_up(self.message.clone()).await,
                }
                .map_err(|error| TurnPolicyError::new(error.to_string()))?;
                self.receipts.borrow_mut().push(receipt);
            }
            Ok(true)
        })
    }
}

struct CaptureRunResumeHook(Arc<Mutex<Vec<RunResumeIntent>>>);

impl RunResumeHook for CaptureRunResumeHook {
    fn before_resume<'a>(
        &'a self,
        intent: &'a RunResumeIntent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|error| HarnessError::Context {
                    message: error.to_string(),
                })?;
            lock(&self.0).push(intent.clone());
            Ok(())
        })
    }
}

struct ObserveSystemPrompt(Arc<Mutex<Vec<String>>>);

impl TurnPolicy for ObserveSystemPrompt {
    fn prepare_next_turn<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.0).push(turn.context.system_prompt.clone());
            Ok(NextTurn::default())
        })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async { Ok(false) })
    }
}

fn accounted_response(
    id: &str,
    content: ContentBlockKind,
    usage: Usage,
    cost_micros: i128,
) -> ScriptedResponse {
    let mut assembler = AssistantAssembler::new();
    let mut events = vec![AssistantEvent::MessageStarted {
        message_id: MessageId::new(id),
        provider: ProviderId::new("scripted"),
        api: ApiId::new("scripted"),
        model: ModelId::new("test-model"),
    }];
    assembler.apply(&events[0]).expect("valid stream start");
    let block_id = ContentBlockId::new(format!("{id}-block"));
    let start = AssistantEvent::ContentBlockStarted {
        block_id: block_id.clone(),
        content_index: 0,
        kind: content,
    };
    assembler.apply(&start).expect("valid content start");
    events.push(start);
    let finish_reason = match content {
        ContentBlockKind::Text => {
            let delta = AssistantEvent::TextDelta {
                block_id: block_id.clone(),
                delta: "done".to_owned(),
            };
            assembler.apply(&delta).expect("valid text delta");
            events.push(delta);
            AssistantFinishReason::Stop
        }
        ContentBlockKind::ToolCall => {
            for event in [
                AssistantEvent::ToolCallMetadata {
                    block_id: block_id.clone(),
                    call_id: ToolCallId::new(format!("{id}-call")),
                    name: Some("echo".to_owned()),
                },
                AssistantEvent::ToolArgumentsDelta {
                    block_id: block_id.clone(),
                    delta: "{}".to_owned(),
                },
            ] {
                assembler.apply(&event).expect("valid tool call event");
                events.push(event);
            }
            AssistantFinishReason::ToolUse
        }
        ContentBlockKind::Thinking => panic!("accounted response does not use thinking"),
    };
    for event in [
        AssistantEvent::ContentBlockFinished { block_id },
        AssistantEvent::UsageUpdated { cumulative: usage },
    ] {
        assembler.apply(&event).expect("valid terminal input");
        events.push(event);
    }
    let mut message = assembler
        .finish_completed(AssistantFinish {
            reason: finish_reason,
            raw_provider_reason: None,
            error: None,
        })
        .expect("complete accounted response");
    message.cost = Some(Cost {
        currency: Currency::usd(),
        micros: cost_micros,
    });
    events.push(AssistantEvent::Finished { message });
    ScriptedResponse::events(events)
}

fn multiple_tool_response() -> ScriptedResponse {
    let mut assembler = AssistantAssembler::new();
    let mut events = vec![AssistantEvent::MessageStarted {
        message_id: MessageId::new("assistant-tools"),
        provider: ProviderId::new("scripted"),
        api: ApiId::new("scripted"),
        model: ModelId::new("test-model"),
    }];
    for event in &events {
        assembler.apply(event).expect("valid stream start");
    }
    for index in 0..2_u32 {
        let block_id = ContentBlockId::new(format!("tool-block-{index}"));
        for event in [
            AssistantEvent::ContentBlockStarted {
                block_id: block_id.clone(),
                content_index: index,
                kind: ContentBlockKind::ToolCall,
            },
            AssistantEvent::ToolCallMetadata {
                block_id: block_id.clone(),
                call_id: ToolCallId::new(format!("call-{index}")),
                name: Some("echo".to_owned()),
            },
            AssistantEvent::ToolArgumentsDelta {
                block_id: block_id.clone(),
                delta: format!("{{\"index\":{index}}}"),
            },
            AssistantEvent::ContentBlockFinished { block_id },
        ] {
            assembler.apply(&event).expect("valid tool event");
            events.push(event);
        }
    }
    let message = assembler
        .finish_completed(AssistantFinish {
            reason: AssistantFinishReason::ToolUse,
            raw_provider_reason: None,
            error: None,
        })
        .expect("complete tool message");
    events.push(AssistantEvent::Finished { message });
    ScriptedResponse::events(events)
}

#[derive(Clone)]
struct EchoTool {
    spec: ToolSpec,
    executions: Arc<AtomicUsize>,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "echo input".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            executions: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_counter(executions: Arc<AtomicUsize>) -> Self {
        Self {
            executions,
            ..Self::new()
        }
    }
}

impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        context: ToolCallContext,
        _updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new(format!("{}-result", context.call.id.as_str())),
                text: "ok".to_owned(),
            }]))
        })
    }
}

#[derive(Clone)]
struct NonIdempotentToolPolicy {
    authorizations: Arc<AtomicUsize>,
}

impl ToolPolicy for NonIdempotentToolPolicy {
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, pi_agent_core::AgentError>> {
        let pass = self.authorizations.fetch_add(1, Ordering::SeqCst) + 1;
        context.args["authorization_pass"] = json!(pass);
        Box::pin(async { Ok(ToolAuthorization::Allow) })
    }

    fn finalize<'a>(
        &'a self,
        _context: AfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, pi_agent_core::AgentError>> {
        Box::pin(async { Ok(ToolOutputPatch::default()) })
    }
}

impl LocalToolPolicy for NonIdempotentToolPolicy {
    fn authorize<'a>(
        &'a self,
        context: LocalBeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, pi_agent_core::AgentError>> {
        let pass = self.authorizations.fetch_add(1, Ordering::SeqCst) + 1;
        context.args["authorization_pass"] = json!(pass);
        Box::pin(async { Ok(ToolAuthorization::Allow) })
    }

    fn finalize<'a>(
        &'a self,
        _context: LocalAfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, pi_agent_core::AgentError>> {
        Box::pin(async { Ok(ToolOutputPatch::default()) })
    }
}

fn prepared_recovery_arguments() -> serde_json::Value {
    json!({
        "value": "raw",
        "preparation_pass": 1,
        "authorization_pass": 1,
    })
}

fn send_non_idempotent_preparer(preparations: Arc<AtomicUsize>) -> Arc<dyn ToolArgumentPreparer> {
    Arc::new(move |arguments: &serde_json::Value| {
        let pass = preparations.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({
            "value": arguments["value"].clone(),
            "preparation_pass": pass,
        }))
    })
}

fn local_non_idempotent_preparer(
    preparations: Arc<AtomicUsize>,
) -> Rc<dyn LocalToolArgumentPreparer> {
    Rc::new(move |arguments: &serde_json::Value| {
        let pass = preparations.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({
            "value": arguments["value"].clone(),
            "preparation_pass": pass,
        }))
    })
}

struct PausingTool {
    spec: ToolSpec,
    observed: Arc<Mutex<Vec<serde_json::Value>>>,
}

struct PausingLocalTool {
    spec: ToolSpec,
    observed: Rc<RefCell<Vec<serde_json::Value>>>,
}

impl PausingLocalTool {
    fn new(observed: Rc<RefCell<Vec<serde_json::Value>>>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "pause local execution after it starts".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            observed,
        }
    }
}

impl LocalTool for PausingLocalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Rc<dyn LocalToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        self.observed.borrow_mut().push(context.call.arguments);
        updates
            .update(ToolUpdate::from(ToolOutput::new(vec![
                ToolResultContent::Text {
                    id: ContentBlockId::new("local-pause-update"),
                    text: "started".to_owned(),
                },
            ])))
            .expect("local update accepted before pause");
        Box::pin(futures_util::future::pending())
    }
}

impl PausingTool {
    fn new(observed: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "pause after execution starts".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            observed,
        }
    }
}

impl Tool for PausingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        lock(&self.observed).push(context.call.arguments);
        updates
            .update(ToolUpdate::from(ToolOutput::new(vec![
                ToolResultContent::Text {
                    id: ContentBlockId::new("pause-update"),
                    text: "started".to_owned(),
                },
            ])))
            .expect("update accepted before pause");
        Box::pin(futures_util::future::pending())
    }
}

struct RecordingLocalTool {
    spec: ToolSpec,
    observed: Rc<RefCell<Vec<serde_json::Value>>>,
}

struct RecordingTool {
    spec: ToolSpec,
    observed: Arc<Mutex<Vec<serde_json::Value>>>,
}

struct GatedRecoveryTool {
    spec: ToolSpec,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl GatedRecoveryTool {
    fn new(started: oneshot::Sender<()>, release: oneshot::Receiver<()>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "gate a recovered tool while ingress is enqueued".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            started: Mutex::new(Some(started)),
            release: Mutex::new(Some(release)),
        }
    }
}

impl Tool for GatedRecoveryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        context: ToolCallContext,
        _updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        let started = lock(&self.started)
            .take()
            .expect("gated recovery tool executes once");
        let release = lock(&self.release)
            .take()
            .expect("gated recovery release is present");
        started
            .send(())
            .expect("recovery start receiver remains live");
        Box::pin(async move {
            release.await.expect("test releases recovered tool");
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new(format!("{}-result", context.call.id.as_str())),
                text: "recovered".to_owned(),
            }]))
        })
    }
}

impl RecordingTool {
    fn new(observed: Arc<Mutex<Vec<serde_json::Value>>>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "record arguments".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            observed,
        }
    }
}

impl Tool for RecordingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        context: ToolCallContext,
        _updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        lock(&self.observed).push(context.call.arguments);
        Box::pin(async {
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new("recording-result"),
                text: "ok".to_owned(),
            }]))
        })
    }
}

impl RecordingLocalTool {
    fn new(observed: Rc<RefCell<Vec<serde_json::Value>>>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".to_owned(),
                description: "record local arguments".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            observed,
        }
    }
}

impl LocalTool for RecordingLocalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute(
        &self,
        context: ToolCallContext,
        _updates: Rc<dyn LocalToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, pi_agent_core::ToolError>> {
        self.observed.borrow_mut().push(context.call.arguments);
        Box::pin(async {
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new("local-result"),
                text: "ok".to_owned(),
            }]))
        })
    }
}

fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::RunStarted { .. } => "run_started",
        AgentEvent::TurnStarted { .. } => "turn_started",
        AgentEvent::ContextPrepared { .. } => "context_prepared",
        AgentEvent::MessageStarted { role, .. } => match role {
            MessageRole::User => "message_started_user",
            MessageRole::Assistant => "message_started_assistant",
            MessageRole::ToolResult => "message_started_tool_result",
            MessageRole::Custom => "message_started_custom",
        },
        AgentEvent::AssistantUpdate { .. } => "assistant_update",
        AgentEvent::MessageCommitted { message } => match message {
            AgentRecord::Llm(Message::User(_)) => "message_committed_user",
            AgentRecord::Llm(Message::Assistant(_)) => "message_committed_assistant",
            AgentRecord::Llm(Message::ToolResult(_)) => "message_committed_tool_result",
            AgentRecord::Custom { .. } => "message_committed_custom",
        },
        AgentEvent::ToolExecutionStarted { .. } => "tool_execution_started",
        AgentEvent::ToolExecutionUpdated { .. } => "tool_execution_updated",
        AgentEvent::ToolExecutionFinished { .. } => "tool_execution_finished",
        AgentEvent::TurnFinished { .. } => "turn_finished",
        AgentEvent::RunFinished { .. } => "run_finished",
        _ => "other",
    }
}

fn message_text(record: &AgentRecord) -> Option<&str> {
    let AgentRecord::Llm(Message::User(message)) = record else {
        return None;
    };
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.as_str()),
        ContentBlock::Image { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::ToolCall { .. } => None,
    })
}

#[test]
fn harness_agent_gate_end_to_end() {
    // Architecture v2 part 2 §7.1, §8.3, and §10.9. Pi basis:
    // harness/reducer.test.ts durable run prefixes plus agent-loop.ts lifecycle.
    let storage = storage("agent-gate");
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique tool");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "gate"),
        agent(
            [
                tool_call_response("echo", json!({"value":"one"})),
                text_response("done"),
            ],
            tools,
        ),
    ))
    .expect("open harness");

    let events = block_on(async {
        harness
            .prompt_records(vec![user("user-1", "go")], CancellationToken::new())
            .await
            .expect("start durable run")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
    })
    .expect("drive durable run");

    assert!(matches!(
        events.first(),
        Some(HarnessRunEvent::Harness(HarnessEvent::RunStart { .. }))
    ));
    assert!(matches!(
        events.last(),
        Some(HarnessRunEvent::Harness(HarnessEvent::RunEnd {
            outcome: HarnessRunOutcome::Completed,
            ..
        }))
    ));
    let names = events
        .iter()
        .filter_map(|event| match event {
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::AssistantUpdate { .. }) =>
            {
                None
            }
            HarnessRunEvent::Agent(event) => Some(event_name(event)),
            HarnessRunEvent::Harness(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "run_started",
            "turn_started",
            "message_started_user",
            "message_committed_user",
            "context_prepared",
            "message_started_assistant",
            "message_committed_assistant",
            "tool_execution_started",
            "tool_execution_finished",
            "message_started_tool_result",
            "message_committed_tool_result",
            "turn_finished",
            "turn_started",
            "context_prepared",
            "message_started_assistant",
            "message_committed_assistant",
            "turn_finished",
            "run_finished",
        ]
    );

    let state = storage.state_snapshot().expect("durable state");
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
    let records = state.records_in_sequence_order();
    assert!(matches!(
        records.first(),
        Some(OperationRecord::Started { .. })
    ));
    assert!(matches!(
        records.last(),
        Some(OperationRecord::Finished {
            outcome: OperationOutcome::Completed,
            ..
        })
    ));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                OperationRecord::StepAttempt {
                    step: OperationStep::Assistant,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, OperationRecord::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(state.entries_in_sequence_order().len(), 4);
}

#[test]
fn harness_failed_assistant_is_committed_before_failed_operation_terminal() {
    // Architecture v2 part 2 §7.1 and §10.9 failure commitment. Pi basis:
    // agent-loop.ts commits the failed assistant before agent_end, while the
    // durable reducer prefix ends only after the result entry exists.
    let storage = storage("failed-run");
    let error = PublicError {
        code: "provider_error".to_owned(),
        message: "provider failed".to_owned(),
        retryable: true,
        provider_code: Some("overloaded".to_owned()),
        status: Some(503),
        request_id: Some("request-1".to_owned()),
    };
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "failed"),
        agent(
            [ScriptedResponse::failure(error.clone())],
            ToolRegistry::new(),
        ),
    ))
    .expect("open harness");
    let events = block_on(async {
        harness
            .prompt_records(vec![user("failure-user", "go")], CancellationToken::new())
            .await
            .expect("start failed run")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert!(matches!(
        events.last(),
        Some(Ok(HarnessRunEvent::Harness(HarnessEvent::RunEnd {
            outcome: HarnessRunOutcome::Failed,
            ..
        })))
    ));
    let state = storage.state_snapshot().expect("failed durable state");
    let assistant_sequence = state
        .entries_in_sequence_order()
        .into_iter()
        .find_map(|entry| match entry {
            pi_agent_session::SessionEntry::Message {
                base,
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } if assistant.finish.error.as_ref() == Some(&error) => Some(base.sequence),
            _ => None,
        })
        .expect("failed assistant committed");
    let finished = state
        .records_in_sequence_order()
        .iter()
        .find(|record| matches!(record, OperationRecord::Finished { .. }))
        .expect("failed operation terminal");
    assert!(assistant_sequence < finished.sequence());
    assert!(matches!(
        finished,
        OperationRecord::Finished {
            outcome: OperationOutcome::Failed,
            error: Some(recorded),
            ..
        } if recorded == &error
    ));
}

#[test]
fn harness_cancel_persists_abort_request_before_aborted_terminal() {
    // Architecture v2 part 2 §7.2 and §8.4. Pi basis: abort is observable to
    // the agent loop, while the native durable harness first records intent.
    let storage = storage("cancel-run");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "cancel"),
        agent([text_response("unused")], ToolRegistry::new()),
    ))
    .expect("open harness");
    let control = harness.control();
    let mut stream =
        block_on(harness.prompt_records(vec![user("cancel-user", "go")], CancellationToken::new()))
            .expect("start cancellable run");
    assert!(matches!(
        block_on(stream.next()),
        Some(Ok(HarnessRunEvent::Harness(HarnessEvent::RunStart { .. })))
    ));
    block_on(control.cancel()).expect("durable cancellation");
    let remaining = block_on(stream.collect::<Vec<_>>());
    assert!(remaining.iter().all(Result::is_ok));
    assert!(matches!(
        remaining.last(),
        Some(Ok(HarnessRunEvent::Harness(HarnessEvent::RunEnd {
            outcome: HarnessRunOutcome::Aborted,
            ..
        })))
    ));
    let state = storage.state_snapshot().expect("cancelled durable state");
    let records = state.records_in_sequence_order();
    let abort_index = records
        .iter()
        .position(|record| matches!(record, OperationRecord::AbortRequested { .. }))
        .expect("abort request record");
    let finish_index = records
        .iter()
        .position(|record| {
            matches!(
                record,
                OperationRecord::Finished {
                    outcome: OperationOutcome::Aborted,
                    ..
                }
            )
        })
        .expect("aborted operation terminal");
    assert!(abort_index < finish_index);
}

#[test]
fn harness_durable_queue_acknowledgement() {
    // Architecture v2 part 2 §8.4. Pi basis: agent.ts queue ingress and
    // harness/session durable queue records. The returned receipt is observed
    // only after QueueEnqueued is accepted by storage.
    let storage = storage("durable-queue");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "queue"),
        agent([text_response("done")], ToolRegistry::new()),
    ))
    .expect("open harness");
    let control = harness.control();
    let mut stream =
        block_on(harness.prompt_records(vec![user("prompt", "initial")], CancellationToken::new()))
            .expect("start run");
    let first = block_on(stream.next()).expect("run start").expect("event");
    assert!(matches!(
        first,
        HarnessRunEvent::Harness(HarnessEvent::RunStart { .. })
    ));

    let receipt = block_on(control.steer(user("steer", "steered"))).expect("durable enqueue");
    let accepted = storage
        .state_snapshot()
        .expect("state after acknowledgement");
    assert!(accepted.records_in_sequence_order().iter().any(|record| {
        matches!(record, OperationRecord::QueueEnqueued { queue: QueueKind::Steer, target, .. } if target.id() == &receipt.entry_id)
    }));
    assert!(accepted.entry(&receipt.entry_id).is_none());
    assert_eq!(accepted.sequence(), receipt.durable.last_sequence);
    assert!(receipt.agent.is_some());

    let remaining = block_on(stream.collect::<Vec<_>>());
    assert!(remaining.iter().all(Result::is_ok));
    let finished = storage.state_snapshot().expect("finished state");
    assert!(finished.entry(&receipt.entry_id).is_some());
    assert!(matches!(
        finished.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
}

#[test]
fn harness_send_acknowledged_queue_survives_should_stop_for_continue() {
    // Architecture v2 part 2 §8.2 and §8.4; §10.9 queue ordering. Pi basis:
    // agent-loop.test.ts "should stop after the current turn" and agent.test.ts
    // assistant-tail continue queue tests. should_stop precedes polling, so an
    // acknowledged queue item remains available to the next continue run.
    for (label, queue) in [
        ("steering", StopQueue::Steering),
        ("follow-up", StopQueue::FollowUp),
    ] {
        let storage = storage(&format!("send-stop-{label}"));
        let control_slot = Arc::new(Mutex::new(None));
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let queued_message = user(&format!("send-{label}-queued"), &format!("queued {label}"));
        let mut core = agent(
            [text_response("first"), text_response("continued")],
            ToolRegistry::new(),
        );
        core.set_turn_policy(Arc::new(QueueDuringStopPolicy {
            control: control_slot.clone(),
            queue,
            message: queued_message,
            enqueued: AtomicBool::new(false),
            receipts: receipts.clone(),
        }))
        .expect("install stop policy");
        let mut harness = block_on(AgentHarness::open(
            session(storage.clone(), &format!("send-stop-{label}")),
            core,
        ))
        .expect("open send harness");
        *lock(&control_slot) = Some(harness.control());

        let first = block_on(async {
            harness
                .prompt_records(
                    vec![user(&format!("send-{label}-prompt"), "initial")],
                    CancellationToken::new(),
                )
                .await
                .expect("start stopped run")
                .collect::<Vec<_>>()
                .await
        });
        assert!(first.iter().all(Result::is_ok));
        let receipt = lock(&receipts)
            .first()
            .cloned()
            .expect("queue acknowledgement from should_stop");
        let stopped = storage.state_snapshot().expect("stopped state");
        assert!(stopped.entry(&receipt.entry_id).is_none());
        assert!(!stopped.records_in_sequence_order().iter().any(|record| {
            matches!(record, OperationRecord::QueueCancelled { entry_id, .. } if entry_id == &receipt.entry_id)
        }));

        let continued = block_on(async {
            harness
                .continue_run(CancellationToken::new())
                .await
                .expect("continue queued item")
                .collect::<Vec<_>>()
                .await
        });
        assert!(continued.iter().all(Result::is_ok));
        assert!(
            storage
                .state_snapshot()
                .expect("continued state")
                .entry(&receipt.entry_id)
                .is_some()
        );
    }
}

#[test]
fn harness_local_acknowledged_queue_survives_should_stop_for_continue() {
    // Local/WASM counterpart of the §8.2/§8.4 successful-stop retention gate.
    for (label, queue) in [
        ("steering", StopQueue::Steering),
        ("follow-up", StopQueue::FollowUp),
    ] {
        let storage = Rc::new(
            InMemorySessionStorage::new(SessionHeader::new(
                format!("local-stop-{label}"),
                Timestamp::from_unix_millis(1),
                SessionEnvironmentMetadata::default(),
            ))
            .expect("valid local stop storage"),
        );
        let control_slot = Rc::new(RefCell::new(None));
        let receipts = Rc::new(RefCell::new(Vec::new()));
        let mut core = local_agent([text_response("first"), text_response("continued")]);
        core.set_turn_policy(Rc::new(LocalQueueDuringStopPolicy {
            control: control_slot.clone(),
            queue,
            message: user(&format!("local-{label}-queued"), &format!("queued {label}")),
            enqueued: RefCell::new(false),
            receipts: receipts.clone(),
        }))
        .expect("install local stop policy");
        let mut harness = block_on(LocalAgentHarness::open(
            local_session(storage.clone(), &format!("local-stop-{label}")),
            core,
        ))
        .expect("open local harness");
        *control_slot.borrow_mut() = Some(harness.control());

        let first = block_on(async {
            harness
                .prompt_records(
                    vec![user(&format!("local-{label}-prompt"), "initial")],
                    CancellationToken::new(),
                )
                .await
                .expect("start local stopped run")
                .collect::<Vec<_>>()
                .await
        });
        assert!(first.iter().all(Result::is_ok));
        let receipt = receipts
            .borrow()
            .first()
            .cloned()
            .expect("local queue acknowledgement from should_stop");
        assert!(
            storage
                .state_snapshot()
                .expect("local stopped state")
                .entry(&receipt.entry_id)
                .is_none()
        );

        let continued = block_on(async {
            harness
                .continue_run(CancellationToken::new())
                .await
                .expect("continue local queued item")
                .collect::<Vec<_>>()
                .await
        });
        assert!(continued.iter().all(Result::is_ok));
        assert!(
            storage
                .state_snapshot()
                .expect("local continued state")
                .entry(&receipt.entry_id)
                .is_some()
        );
    }
}

#[test]
fn harness_next_run_queue_survives_until_prompt_acceptance() {
    // Architecture v2 part 2 §8.4. Pi basis: harness queue records and the
    // high-level next-run queue boundary.
    let storage = storage("next-run");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "next"),
        agent([text_response("done")], ToolRegistry::new()),
    ))
    .expect("open harness");
    let queued = block_on(harness.control().next_run(user("queued", "first")))
        .expect("durable next-run enqueue");
    assert!(
        storage
            .state_snapshot()
            .expect("queued state")
            .entry(&queued.entry_id)
            .is_none()
    );

    let events = block_on(async {
        harness
            .prompt_records(vec![user("caller", "second")], CancellationToken::new())
            .await
            .expect("start run")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    let durable = storage.state_snapshot().expect("finished state");
    let user_text = durable
        .entries_in_sequence_order()
        .into_iter()
        .filter_map(|entry| match entry {
            pi_agent_session::SessionEntry::Message { message, .. } => message_text(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user_text, ["first", "second"]);
}

#[test]
fn harness_acknowledged_next_run_enqueue_is_captured_by_prompt() {
    // Architecture v2 part 2 §8.4. The queue append is durably accepted while
    // its acknowledgement is paused; the shared transition gate prevents the
    // prompt from accepting an operation intent that omits that enqueue.
    let inner = storage("next-run-prompt-race");
    let (pausing, appended, release) = PausingQueueStorage::new(inner.clone());
    let durable: Arc<dyn SessionStorage> = pausing;
    let mut harness = block_on(AgentHarness::open(
        session(durable, "next-run-race"),
        agent([text_response("done")], ToolRegistry::new()),
    ))
    .expect("open race harness");
    let control = harness.control();
    let (queued, stream, ()) = block_on(async {
        futures_util::future::join3(
            control.next_run(user("raced-next-run", "queued first")),
            harness.prompt_records(
                vec![user("race-caller", "caller second")],
                CancellationToken::new(),
            ),
            async move {
                appended.await.expect("queue append signal");
                release.send(()).expect("release queue acknowledgement");
            },
        )
        .await
    });
    let queued = queued.expect("acknowledged next-run input");
    let events = block_on(stream.expect("accepted prompt").collect::<Vec<_>>());
    assert!(events.iter().all(Result::is_ok));
    let state = inner.state_snapshot().expect("race state");
    let (queue_sequence, start_sequence, captured) =
        state
            .records_in_sequence_order()
            .iter()
            .fold((None, None, false), |mut found, record| {
                match record {
                    OperationRecord::QueueEnqueued { base, target, .. }
                        if target.id() == &queued.entry_id =>
                    {
                        found.0 = Some(base.sequence);
                    }
                    OperationRecord::Started { base, intent, .. } => {
                        found.1 = Some(base.sequence);
                        if let OperationIntent::Run {
                            initial_messages, ..
                        } = intent
                        {
                            found.2 = initial_messages
                                .iter()
                                .any(|target| target.id() == &queued.entry_id);
                        }
                    }
                    _ => {}
                }
                found
            });
    assert!(captured);
    assert!(queue_sequence.expect("queue sequence") < start_sequence.expect("start sequence"));
}

#[test]
fn harness_local_acknowledged_next_run_enqueue_is_captured_by_prompt() {
    // Local/WASM counterpart of the §8.4 transition-gate race above.
    let inner = storage("local-next-run-prompt-race");
    let (pausing, appended, release) = PausingQueueStorage::new_local(inner.clone());
    let durable: Rc<dyn LocalSessionStorage> = pausing;
    let mut harness = block_on(LocalAgentHarness::open(
        local_session(durable, "local-next-run-race"),
        local_agent([text_response("done")]),
    ))
    .expect("open local race harness");
    let control = harness.control();
    let (queued, stream, ()) = block_on(async {
        futures_util::future::join3(
            control.next_run(user("local-raced-next-run", "queued first")),
            harness.prompt_records(
                vec![user("local-race-caller", "caller second")],
                CancellationToken::new(),
            ),
            async move {
                appended.await.expect("local queue append signal");
                release
                    .send(())
                    .expect("release local queue acknowledgement");
            },
        )
        .await
    });
    let queued = queued.expect("acknowledged local next-run input");
    let events = block_on(stream.expect("accepted local prompt").collect::<Vec<_>>());
    assert!(events.iter().all(Result::is_ok));
    let state = inner.state_snapshot().expect("local race state");
    let captured = state.records_in_sequence_order().iter().any(|record| {
        matches!(
            record,
            OperationRecord::Started {
                intent: OperationIntent::Run { initial_messages, .. },
                ..
            } if initial_messages.iter().any(|target| target.id() == &queued.entry_id)
        )
    });
    assert!(captured);
}

#[test]
fn harness_operation_recovery_resumes_recorded_intent() {
    // Architecture v2 part 2 §7.6 and §10.10
    // session_operation_recovery_reconstructs_intent. Pi basis:
    // harness/session/state.ts getOpenOperations plus reducer valid prefixes.
    let storage = storage("recover");
    let intended = user("recorded-prompt", "recover me");
    let initial = ProvisionedEntry::Message {
        id: EntryId::new("recorded-entry"),
        message: intended.clone(),
        terminate: false,
    };
    storage
        .append_batch(
            Sequence::ZERO,
            vec![operation_started(
                Sequence::new(1),
                "recorded-operation",
                OperationIntent::Run {
                    original_prompt: vec![intended],
                    initial_messages: vec![initial],
                    system_prompt_override: None,
                    resume_data: Default::default(),
                },
            )],
        )
        .expect("admit interrupted operation");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "recover"),
        agent([text_response("resumed")], ToolRegistry::new()),
    ))
    .expect("recover harness");
    assert!(matches!(
        harness.recovery(),
        RecoveryDecision::Resume { .. }
    ));

    let events = block_on(async {
        harness
            .resume_run(CancellationToken::new())
            .await
            .expect("resume recorded run")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    let state = storage.state_snapshot().expect("recovered state");
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
    assert_eq!(state.entries_in_sequence_order().len(), 2);
}

#[test]
fn harness_recovery_reapplies_complete_run_intent() {
    // Architecture v2 part 2 §7.6 and §10.10
    // session_operation_recovery_reconstructs_intent. Pi basis:
    // harness/session/types.ts Run intent and agent-harness.ts before_resume.
    let storage = storage("recover-full-intent");
    let original = user("original-prompt", "original");
    let prepared = user("prepared-prompt", "prepared");
    let initial = ProvisionedEntry::Message {
        id: EntryId::new("prepared-entry"),
        message: prepared,
        terminate: false,
    };
    let extension = VersionedExtension {
        schema_version: 7,
        value: to_raw_value(&json!({"cursor":"resume-here"})).expect("raw resume data"),
    };
    let resume_data = BTreeMap::from([("test.extension".to_owned(), extension)]);
    storage
        .append_batch(
            Sequence::ZERO,
            vec![operation_started(
                Sequence::new(1),
                "full-intent-operation",
                OperationIntent::Run {
                    original_prompt: vec![original.clone()],
                    initial_messages: vec![initial.clone()],
                    system_prompt_override: Some("recovered system prompt".to_owned()),
                    resume_data: resume_data.clone(),
                },
            )],
        )
        .expect("admit full interrupted intent");
    let observed_system = Arc::new(Mutex::new(Vec::new()));
    let mut core = agent([text_response("resumed")], ToolRegistry::new());
    core.set_turn_policy(Arc::new(ObserveSystemPrompt(observed_system.clone())))
        .expect("install system prompt observer");
    let mut harness = block_on(AgentHarness::open(
        session(storage, "recover-full-intent"),
        core,
    ))
    .expect("open full-intent recovery");
    let observed_intent = Arc::new(Mutex::new(Vec::new()));
    harness.set_run_resume_hook(Arc::new(CaptureRunResumeHook(observed_intent.clone())));

    let events = block_on(async {
        harness
            .resume_run(CancellationToken::new())
            .await
            .expect("resume full intent")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        lock(&observed_intent).as_slice(),
        [RunResumeIntent {
            run_id: RunId::new("full-intent-operation"),
            original_prompt: vec![original],
            initial_messages: vec![initial],
            system_prompt_override: Some("recovered system prompt".to_owned()),
            resume_data,
        }]
    );
    assert_eq!(
        lock(&observed_system).as_slice(),
        ["recovered system prompt"]
    );
    assert_eq!(harness.agent().state().system_prompt, "You are helpful.");
}

#[test]
fn harness_incomplete_assistant_recovery_defers_later_queue_until_after_recovered_turn() {
    // Architecture v2 part 2 §7.6, §8.2, and §8.4. Pi basis: reducer.ts
    // sequence-aware open-operation state plus agent-loop.ts queue polling
    // after a complete turn. A queue accepted after StepAttempt cannot enter
    // the request whose attempt was already recorded.
    let storage = storage("recover-attempt-queue-order");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "attempt-first"),
        agent([text_response("unused")], ToolRegistry::new()),
    ))
    .expect("open first attempt harness");
    let control = first.control();
    let mut interrupted = block_on(first.prompt_records(
        vec![user("attempt-user", "initial")],
        CancellationToken::new(),
    ))
    .expect("start interrupted assistant attempt");
    loop {
        let event = block_on(interrupted.next())
            .expect("context preparation event")
            .expect("valid first-process event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::ContextPrepared { .. })
        ) {
            break;
        }
    }
    let queued = block_on(control.steer(user("after-attempt-queue", "after attempt")))
        .expect("enqueue after assistant attempt");
    let prefix = storage.state_snapshot().expect("interrupted prefix");
    let attempt_sequence = prefix
        .records_in_sequence_order()
        .iter()
        .find_map(|record| match record {
            OperationRecord::StepAttempt { base, .. } => Some(base.sequence),
            _ => None,
        })
        .expect("assistant step attempt");
    let queue_sequence = prefix
        .records_in_sequence_order()
        .iter()
        .find_map(|record| match record {
            OperationRecord::QueueEnqueued { base, target, .. }
                if target.id() == &queued.entry_id =>
            {
                Some(base.sequence)
            }
            _ => None,
        })
        .expect("later queue record");
    assert!(attempt_sequence < queue_sequence);
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "attempt-resume"),
        agent(
            [
                text_response("recovered attempted turn"),
                text_response("after queued steering"),
            ],
            ToolRegistry::new(),
        ),
    ))
    .expect("open incomplete-attempt recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume incomplete assistant")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    let messages = storage
        .state_snapshot()
        .expect("recovered ordered state")
        .entries_in_sequence_order()
        .into_iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        messages.as_slice(),
        [
            AgentRecord::Llm(Message::User(_)),
            AgentRecord::Llm(Message::Assistant(_)),
            AgentRecord::Llm(Message::User(_)),
            AgentRecord::Llm(Message::Assistant(_)),
        ]
    ));
    assert_eq!(message_text(&messages[2]), Some("after attempt"));
}

#[test]
fn harness_multi_turn_recovery_reconstructs_aggregate_usage_and_cost() {
    // Architecture v2 part 2 §7.6 and §10.10 operation recovery. Pi basis:
    // agent-loop.ts returns the entire logical run's newMessages; the Rust
    // outcome additionally retains exact fixed-point aggregate accounting.
    let storage = storage("recover-run-accounting");
    let first_usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        reasoning_tokens: Some(1),
        cache_read_tokens: Some(3),
        cache_write_tokens: None,
        cache_write_one_hour_tokens: None,
        total_tokens: Some(15),
        source: UsageSource::ProviderReported,
    };
    let second_usage = Usage {
        input_tokens: 20,
        output_tokens: 5,
        reasoning_tokens: Some(2),
        cache_read_tokens: None,
        cache_write_tokens: Some(4),
        cache_write_one_hour_tokens: Some(1),
        total_tokens: Some(29),
        source: UsageSource::ProviderReported,
    };
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique accounted tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "accounting-first"),
        agent(
            [
                accounted_response(
                    "accounted-tool-turn",
                    ContentBlockKind::ToolCall,
                    first_usage,
                    125,
                ),
                accounted_response(
                    "accounted-final-turn",
                    ContentBlockKind::Text,
                    second_usage,
                    375,
                ),
            ],
            tools,
        ),
    ))
    .expect("open accounting harness");
    let mut interrupted = block_on(first.prompt_records(
        vec![user("accounting-user", "run two turns")],
        CancellationToken::new(),
    ))
    .expect("start accounted run");
    let mut committed_assistants = 0;
    while committed_assistants < 2 {
        let event = block_on(interrupted.next())
            .expect("assistant commitment")
            .expect("valid accounted event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            committed_assistants += 1;
        }
    }
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage, "accounting-resume"),
        agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
    ))
    .expect("open accounting recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("finish accounted recovery")
            .collect::<Vec<_>>()
            .await
    });
    let outcome = events
        .into_iter()
        .find_map(|event| match event.expect("valid recovery event") {
            HarnessRunEvent::Agent(event) => match event.as_ref() {
                AgentEvent::RunFinished { outcome } => Some(outcome.clone()),
                _ => None,
            },
            HarnessRunEvent::Harness(_) => None,
        })
        .expect("recovered run outcome");
    let pi_agent_core::RunOutcome::Completed { usage, cost, .. } = outcome else {
        panic!("expected completed recovered run");
    };
    assert_eq!(
        usage,
        Usage {
            input_tokens: 30,
            output_tokens: 7,
            reasoning_tokens: Some(3),
            cache_read_tokens: Some(3),
            cache_write_tokens: Some(4),
            cache_write_one_hour_tokens: Some(1),
            total_tokens: Some(44),
            source: UsageSource::ProviderReported,
        }
    );
    assert_eq!(
        cost,
        Some(Cost {
            currency: Currency::usd(),
            micros: 500,
        })
    );
}

#[test]
fn harness_completed_resume_becomes_idle_without_replaying_assistant() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.test.ts derives
    // `newestOwn` and treats a committed successful assistant as an operation
    // awaiting only operation_finished.
    let storage = storage("recover-completed");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "completed-first"),
        agent([text_response("already committed")], ToolRegistry::new()),
    ))
    .expect("open first harness");
    let mut interrupted = block_on(
        first.prompt_records(vec![user("completed-user", "go")], CancellationToken::new()),
    )
    .expect("start first run");
    loop {
        let event = block_on(interrupted.next())
            .expect("assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "completed-resume"),
        agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
    ))
    .expect("open interrupted operation");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("finish committed assistant")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert!(matches!(resumed.recovery(), RecoveryDecision::Idle));
    let state = storage.state_snapshot().expect("terminal state");
    assert_eq!(state.entries_in_sequence_order().len(), 2);
    assert!(matches!(
        state.records_in_sequence_order().last(),
        Some(OperationRecord::Finished {
            outcome: OperationOutcome::Completed,
            ..
        })
    ));
}

#[test]
fn harness_recovery_continues_after_newest_consumed_input() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.ts derives `newestOwn`
    // from the exact operation tail. This is the crash prefix after a steering
    // entry commits and before the next assistant attempt starts.
    let storage = storage("recover-consumed-input");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "consumed-first"),
        agent([text_response("first")], ToolRegistry::new()),
    ))
    .expect("open first harness");
    let control = first.control();
    let mut interrupted = block_on(first.prompt_records(
        vec![user("initial-user", "initial")],
        CancellationToken::new(),
    ))
    .expect("start first run");
    loop {
        let event = block_on(interrupted.next())
            .expect("first assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    block_on(control.steer(user("consumed-steer", "continue this intent")))
        .expect("durable steering input");
    loop {
        let event = block_on(interrupted.next())
            .expect("steering commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted { message }
                    if message_text(message) == Some("continue this intent"))
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "consumed-resume"),
        agent([text_response("continued")], ToolRegistry::new()),
    ))
    .expect("open consumed-input prefix");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume after consumed input")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    let recovered_state = storage.state_snapshot().expect("recovered state");
    let assistant_texts = recovered_state
        .entries_in_sequence_order()
        .into_iter()
        .filter_map(|entry| match entry {
            pi_agent_session::SessionEntry::Message {
                message: AgentRecord::Llm(Message::Assistant(assistant)),
                ..
            } => assistant.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                ContentBlock::Image { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolCall { .. } => None,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_texts, ["first", "continued"]);
}

#[test]
fn harness_failed_resume_closes_failed_operation_without_retry() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.test.ts derives
    // `terminalFailure`; a committed failed assistant is terminal durable
    // output and must not be requested again.
    let storage = storage("recover-failed");
    let failure = PublicError {
        code: "provider_error".to_owned(),
        message: "committed failure".to_owned(),
        retryable: true,
        provider_code: None,
        status: Some(503),
        request_id: None,
    };
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "failed-first"),
        agent(
            [ScriptedResponse::failure(failure.clone())],
            ToolRegistry::new(),
        ),
    ))
    .expect("open first harness");
    let mut interrupted =
        block_on(first.prompt_records(vec![user("failed-user", "go")], CancellationToken::new()))
            .expect("start failed run");
    loop {
        let event = block_on(interrupted.next())
            .expect("failed assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(assistant))
                } if assistant.finish.reason == pi_ai::AssistantFinishReason::Error)
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "failed-resume"),
        agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
    ))
    .expect("open failed operation");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("close failed operation")
            .collect::<Vec<_>>()
            .await
    });
    assert!(matches!(
        events.last(),
        Some(Ok(HarnessRunEvent::Harness(HarnessEvent::RunEnd {
            outcome: HarnessRunOutcome::Failed,
            ..
        })))
    ));
    assert!(matches!(resumed.recovery(), RecoveryDecision::Idle));
    let state = storage.state_snapshot().expect("terminal failed state");
    assert_eq!(state.entries_in_sequence_order().len(), 2);
    assert!(matches!(
        state.records_in_sequence_order().last(),
        Some(OperationRecord::Finished {
            outcome: OperationOutcome::Failed,
            error: Some(error),
            ..
        }) if error == &failure
    ));
}

#[test]
fn harness_failed_terminal_recovery_clears_restored_core_queues() {
    // Architecture v2 part 2 §7.6 and §8.4. A terminal recovery cancels the
    // durable queue records and reconciles the queue copy restored into core.
    let storage = storage("recover-failed-queue");
    let failure = PublicError {
        code: "provider_error".to_owned(),
        message: "committed failure".to_owned(),
        retryable: false,
        provider_code: None,
        status: Some(500),
        request_id: None,
    };
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "failed-queue-first"),
        agent([ScriptedResponse::failure(failure)], ToolRegistry::new()),
    ))
    .expect("open first harness");
    let control = first.control();
    let mut interrupted = block_on(first.prompt_records(
        vec![user("failed-queue-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start failed run");
    loop {
        let event = block_on(interrupted.next())
            .expect("failed assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(assistant))
                } if assistant.finish.reason == AssistantFinishReason::Error)
        ) {
            break;
        }
    }
    block_on(control.steer(user("stale-failed-steer", "stale failed input")))
        .expect("durable pending queue");
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "failed-queue-resume"),
        agent([text_response("fresh response")], ToolRegistry::new()),
    ))
    .expect("open terminal recovery");
    let terminal = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("finish failed recovery")
            .collect::<Vec<_>>()
            .await
    });
    assert!(terminal.iter().all(Result::is_ok));
    let fresh = block_on(async {
        resumed
            .prompt_records(
                vec![user("fresh-user", "fresh input")],
                CancellationToken::new(),
            )
            .await
            .expect("start fresh run")
            .collect::<Vec<_>>()
            .await
    });
    assert!(fresh.iter().all(Result::is_ok));
    let state = storage.state_snapshot().expect("fresh durable state");
    assert!(!state.entries_in_sequence_order().iter().any(|entry| {
        matches!(
            entry,
            pi_agent_session::SessionEntry::Message { message, .. }
                if message_text(message) == Some("stale failed input")
        )
    }));
}

#[test]
fn harness_abandon_recovery_clears_restored_core_queues() {
    // Architecture v2 part 2 §7.6 and §8.4. Explicit abandonment has the same
    // durable/in-memory queue reconciliation boundary as terminal recovery.
    let storage = storage("abandon-queue");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "abandon-first"),
        agent([text_response("committed")], ToolRegistry::new()),
    ))
    .expect("open first harness");
    let control = first.control();
    let mut interrupted =
        block_on(first.prompt_records(vec![user("abandon-user", "go")], CancellationToken::new()))
            .expect("start abandoned run");
    loop {
        let event = block_on(interrupted.next())
            .expect("assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    block_on(control.follow_up(user("stale-abandon-follow-up", "stale abandoned input")))
        .expect("durable pending follow-up");
    drop(interrupted);
    drop(first);

    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "abandon-resume"),
        agent([text_response("fresh response")], ToolRegistry::new()),
    ))
    .expect("open abandoned recovery");
    block_on(resumed.abandon_recovery(PublicError {
        code: "abandoned".to_owned(),
        message: "explicitly abandoned".to_owned(),
        retryable: false,
        provider_code: None,
        status: None,
        request_id: None,
    }))
    .expect("abandon recovery");
    let fresh = block_on(async {
        resumed
            .prompt_records(
                vec![user("fresh-after-abandon", "fresh input")],
                CancellationToken::new(),
            )
            .await
            .expect("start fresh run")
            .collect::<Vec<_>>()
            .await
    });
    assert!(fresh.iter().all(Result::is_ok));
    let state = storage.state_snapshot().expect("fresh durable state");
    assert!(!state.entries_in_sequence_order().iter().any(|entry| {
        matches!(
            entry,
            pi_agent_session::SessionEntry::Message { message, .. }
                if message_text(message) == Some("stale abandoned input")
        )
    }));
}

#[test]
fn harness_recovery_executes_unstarted_tool_batch_then_continues() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.test.ts `toolBatch`
    // prefixes distinguish an assistant call with no tool_started record from
    // a replay-sensitive started invocation.
    let storage = storage("recover-unstarted-tool");
    let first_counter = Arc::new(AtomicUsize::new(0));
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(EchoTool::with_counter(first_counter.clone())))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "tool-first"),
        agent(
            [tool_call_response("echo", json!({"value":"one"}))],
            first_tools,
        ),
    ))
    .expect("open first harness");
    let mut interrupted =
        block_on(first.prompt_records(vec![user("tool-user", "go")], CancellationToken::new()))
            .expect("start tool run");
    loop {
        let event = block_on(interrupted.next())
            .expect("tool assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);
    assert_eq!(first_counter.load(Ordering::SeqCst), 0);

    let recovery_counter = Arc::new(AtomicUsize::new(0));
    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(EchoTool::with_counter(recovery_counter.clone())))
        .expect("unique recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "tool-resume"),
        agent([text_response("after tool")], recovery_tools),
    ))
    .expect("open tool recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("recover tool batch")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(recovery_counter.load(Ordering::SeqCst), 1);
    let state = storage.state_snapshot().expect("recovered tool state");
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
    assert_eq!(
        state
            .records_in_sequence_order()
            .iter()
            .filter(|record| matches!(record, OperationRecord::ToolStarted { .. }))
            .count(),
        1
    );
}

#[test]
fn harness_recovery_never_replays_started_unsafe_tool() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.test.ts X3 tool prefix
    // plus the durable ToolReplayPolicy contract. A started `never` tool is
    // closed with an error result instead of repeating its side effects.
    let storage = storage("recover-started-tool");
    let first_observed = Arc::new(Mutex::new(Vec::new()));
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(PausingTool::new(first_observed.clone())))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "started-first"),
        agent(
            [tool_call_response("echo", json!({"value":"one"}))],
            first_tools,
        ),
    ))
    .expect("open first harness");
    let mut interrupted =
        block_on(first.prompt_records(vec![user("started-user", "go")], CancellationToken::new()))
            .expect("start tool run");
    loop {
        let event = block_on(interrupted.next())
            .expect("tool start")
            .expect("valid event");
        if matches!(event, HarnessRunEvent::Agent(event)
            if matches!(event.as_ref(), AgentEvent::ToolExecutionUpdated { .. }))
        {
            break;
        }
    }
    drop(interrupted);
    drop(first);
    assert_eq!(lock(&first_observed).as_slice(), [json!({"value":"one"})]);

    let recovery_counter = Arc::new(AtomicUsize::new(0));
    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(EchoTool::with_counter(recovery_counter.clone())))
        .expect("unique recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "started-resume"),
        agent([text_response("handled interruption")], recovery_tools),
    ))
    .expect("open started-tool recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("recover unsafe tool")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(recovery_counter.load(Ordering::SeqCst), 0);
    let state = storage.state_snapshot().expect("unsafe recovery state");
    assert!(state.entries_in_sequence_order().iter().any(|entry| {
        matches!(
            entry,
            pi_agent_session::SessionEntry::Message {
                message: AgentRecord::Llm(Message::ToolResult(result)),
                ..
            } if result.is_error
                && result.content.iter().any(|content| matches!(
                    content,
                    ToolResultContent::Text { text, .. } if text.contains("not replay-safe")
                ))
        )
    }));
}

#[test]
fn harness_configured_safe_tool_replays_after_started_crash() {
    // Architecture v2 part 2 §7.6. Pi basis: HarnessTool.replay in
    // harness/agent-harness.ts. The public harness configuration is recorded
    // on ToolStarted and controls the next process's recovery decision.
    let storage = storage("recover-safe-tool");
    let first_observed = Arc::new(Mutex::new(Vec::new()));
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(PausingTool::new(first_observed.clone())))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "safe-first"),
        agent(
            [tool_call_response("echo", json!({"value":"one"}))],
            first_tools,
        ),
    ))
    .expect("open first harness");
    first.set_tool_replay_policy("echo", ToolReplayPolicy::Safe);
    assert_eq!(first.tool_replay_policy("echo"), ToolReplayPolicy::Safe);
    let mut interrupted =
        block_on(first.prompt_records(vec![user("safe-user", "go")], CancellationToken::new()))
            .expect("start safe tool run");
    loop {
        let event = block_on(interrupted.next())
            .expect("safe tool start")
            .expect("valid event");
        if matches!(event, HarnessRunEvent::Agent(event)
            if matches!(event.as_ref(), AgentEvent::ToolExecutionUpdated { .. }))
        {
            break;
        }
    }
    drop(interrupted);
    drop(first);
    assert_eq!(lock(&first_observed).as_slice(), [json!({"value":"one"})]);
    assert!(
        storage
            .state_snapshot()
            .expect("safe start state")
            .records_in_sequence_order()
            .iter()
            .any(|record| matches!(
                record,
                OperationRecord::ToolStarted {
                    replay: ToolReplayPolicy::Safe,
                    ..
                }
            ))
    );

    let recovery_counter = Arc::new(AtomicUsize::new(0));
    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(EchoTool::with_counter(recovery_counter.clone())))
        .expect("unique recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "safe-resume"),
        agent([text_response("after replay")], recovery_tools),
    ))
    .expect("open safe recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume safe invocation")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(recovery_counter.load(Ordering::SeqCst), 1);
}

#[test]
fn harness_tool_started_replays_final_authorized_arguments() {
    // Architecture v2 part 2 §7.2 and §7.6. Pi basis:
    // agent-loop.ts prepareToolCall passes prepared/validated arguments through
    // beforeToolCall before execution, while reducer.test.ts treats
    // tool_started.effectiveArgs as the resumable invocation intent. Both
    // phases are deliberately non-idempotent here so a recovery regression
    // cannot pass merely because re-running preflight produces the same JSON.
    let storage = storage("recover-authorized-args");
    let preparations = Arc::new(AtomicUsize::new(0));
    let authorizations = Arc::new(AtomicUsize::new(0));
    let first_observed = Arc::new(Mutex::new(Vec::new()));
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register_with_argument_preparer(
            Arc::new(PausingTool::new(first_observed.clone())),
            send_non_idempotent_preparer(preparations.clone()),
        )
        .expect("unique first tool");
    let mut first_agent = agent(
        [tool_call_response("echo", json!({"value":"raw"}))],
        first_tools,
    );
    first_agent
        .set_tool_policy(Arc::new(NonIdempotentToolPolicy {
            authorizations: authorizations.clone(),
        }))
        .expect("idle policy replacement");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "authorized-first"),
        first_agent,
    ))
    .expect("open first harness");
    first.set_tool_replay_policy("echo", ToolReplayPolicy::Safe);
    let mut interrupted = block_on(first.prompt_records(
        vec![user("authorized-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start authorized tool run");
    loop {
        let event = block_on(interrupted.next())
            .expect("tool execution update")
            .expect("valid event");
        if matches!(event, HarnessRunEvent::Agent(event)
            if matches!(event.as_ref(), AgentEvent::ToolExecutionUpdated { .. }))
        {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    assert_eq!(
        lock(&first_observed).as_slice(),
        [prepared_recovery_arguments()]
    );
    let first_state = storage.state_snapshot().expect("authorized start state");
    assert!(
        first_state
            .records_in_sequence_order()
            .iter()
            .any(|record| {
                matches!(
                    record,
                    OperationRecord::ToolStarted {
                        effective_args,
                        replay: ToolReplayPolicy::Safe,
                        ..
                    } if effective_args == &prepared_recovery_arguments()
                )
            })
    );

    let replay_observed = Arc::new(Mutex::new(Vec::new()));
    let mut replay_tools = ToolRegistry::new();
    replay_tools
        .register_with_argument_preparer(
            Arc::new(RecordingTool::new(replay_observed.clone())),
            send_non_idempotent_preparer(preparations.clone()),
        )
        .expect("unique replay tool");
    let mut replay_agent = agent([text_response("after replay")], replay_tools);
    replay_agent
        .set_tool_policy(Arc::new(NonIdempotentToolPolicy {
            authorizations: authorizations.clone(),
        }))
        .expect("idle replay policy replacement");
    let mut resumed = block_on(AgentHarness::open(
        session(storage, "authorized-resume"),
        replay_agent,
    ))
    .expect("open replay harness");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume authorized invocation")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        lock(&replay_observed).as_slice(),
        [prepared_recovery_arguments()]
    );
    assert_eq!(preparations.load(Ordering::SeqCst), 1);
    assert_eq!(authorizations.load(Ordering::SeqCst), 1);
}

#[test]
fn harness_recovered_sequential_tool_lifecycles_precede_next_start() {
    // Architecture v2 part 2 §8.2 recovery ordering. Pi basis: agent-loop.ts
    // executeToolCallsSequential emits execution end and the tool-result
    // message lifecycle before starting the next source-order call.
    let storage = storage("recover-sequential-order");
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "sequential-first"),
        agent([multiple_tool_response()], first_tools),
    ))
    .expect("open first harness");
    let mut interrupted = block_on(first.prompt_records(
        vec![user("sequential-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start sequential run");
    loop {
        let event = block_on(interrupted.next())
            .expect("tool assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "sequential-resume"),
        agent([text_response("after tools")], recovery_tools),
    ))
    .expect("open sequential recovery");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume sequential batch")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
    })
    .expect("drive recovered batch");
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            HarnessRunEvent::Agent(event) => Some(event_name(event)),
            HarnessRunEvent::Harness(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        &lifecycle[..11],
        [
            "run_started",
            "tool_execution_started",
            "tool_execution_finished",
            "message_started_tool_result",
            "message_committed_tool_result",
            "tool_execution_started",
            "tool_execution_finished",
            "message_started_tool_result",
            "message_committed_tool_result",
            "turn_finished",
            "turn_started",
        ]
    );
    assert_eq!(
        &lifecycle[lifecycle.len() - 3..],
        [
            "message_committed_assistant",
            "turn_finished",
            "run_finished",
        ]
    );

    let state = storage.state_snapshot().expect("sequential durable state");
    let starts = state
        .records_in_sequence_order()
        .iter()
        .filter_map(|record| match record {
            OperationRecord::ToolStarted {
                base, tool_index, ..
            } => Some((*tool_index, base.sequence)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let results = state
        .entries_in_sequence_order()
        .iter()
        .filter_map(|entry| match entry {
            pi_agent_session::SessionEntry::Message {
                base,
                message: AgentRecord::Llm(Message::ToolResult(result)),
                ..
            } => Some((result.tool_call_id.clone(), base.sequence)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(results.len(), 2);
    assert!(starts[0].1 < results[0].1 && results[0].1 < starts[1].1);
    assert!(starts[1].1 < results[1].1);
}

#[test]
fn harness_recovery_accepts_concurrent_durable_ingress_while_tools_execute() {
    // Architecture v2 part 2 §8.2–§8.4 and §10.9. Pi basis:
    // packages/agent/src/agent-loop.ts keeps the run active throughout tool
    // execution and polls steering, then follow-up, only after turn_end and
    // the post-turn policies. Queue acknowledgements are durable before this
    // test releases the recovered tool.
    let storage = storage("recover-active-ingress");
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(storage.clone(), "active-ingress-first"),
        agent(
            [tool_call_response("echo", json!({"value":"recover"}))],
            first_tools,
        ),
    ))
    .expect("open first harness");
    let mut interrupted = block_on(first.prompt_records(
        vec![user("active-ingress-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start recoverable run");
    loop {
        let event = block_on(interrupted.next())
            .expect("assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(GatedRecoveryTool::new(started_tx, release_rx)))
        .expect("unique gated recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(storage.clone(), "active-ingress-resume"),
        agent(
            [
                text_response("after steering"),
                text_response("after follow-up"),
            ],
            recovery_tools,
        ),
    ))
    .expect("open recovery harness");
    let control = resumed.control();
    let stream = block_on(resumed.resume_run(CancellationToken::new()))
        .expect("create active recovery stream");
    let (events, (steering, follow_up)) = block_on(futures_util::future::join(
        stream.collect::<Vec<_>>(),
        async move {
            started_rx.await.expect("recovered tool starts");
            let steering = control
                .steer(user("during-recovery-steer", "steer while tool runs"))
                .await;
            let follow_up = control
                .follow_up(user(
                    "during-recovery-follow-up",
                    "follow up while tool runs",
                ))
                .await;
            release_tx.send(()).expect("release recovered tool");
            (steering, follow_up)
        },
    ));
    let steering = steering.expect("durable steering acknowledgement");
    let follow_up = follow_up.expect("durable follow-up acknowledgement");
    assert!(steering.agent.is_some());
    assert!(follow_up.agent.is_some());
    let events = events
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("drive recovery with concurrent ingress");
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            HarnessRunEvent::Agent(event) => Some(event_name(event)),
            HarnessRunEvent::Harness(_) => None,
        })
        .collect::<Vec<_>>();
    let recovered_turn_finished = lifecycle
        .iter()
        .position(|event| *event == "turn_finished")
        .expect("recovered turn finishes");
    let next_turn_started = lifecycle
        .iter()
        .enumerate()
        .find_map(|(index, event)| {
            (index > recovered_turn_finished && *event == "turn_started").then_some(index)
        })
        .expect("queued steering starts a next turn");
    assert!(recovered_turn_finished < next_turn_started);
    assert_eq!(
        lifecycle
            .iter()
            .filter(|event| **event == "message_committed_user")
            .count(),
        2
    );

    let state = storage.state_snapshot().expect("durable ingress state");
    assert_eq!(
        state
            .records_in_sequence_order()
            .iter()
            .filter(|record| matches!(
                record,
                OperationRecord::QueueEnqueued {
                    queue: QueueKind::Steer | QueueKind::FollowUp,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn harness_recovered_sequential_crash_does_not_mark_later_call_started() {
    // Architecture v2 part 2 §8.2 and §7.6. A simulated crash immediately
    // after the first recovered result commit must leave the second call
    // unstarted, so recovery never misclassifies it as an unsafe replay.
    let inner = storage("recover-sequential-crash");
    let mut first_tools = ToolRegistry::new();
    first_tools
        .register(Arc::new(EchoTool::new()))
        .expect("unique first tool");
    let mut first = block_on(AgentHarness::open(
        session(inner.clone(), "sequential-crash-first"),
        agent([multiple_tool_response()], first_tools),
    ))
    .expect("open first harness");
    let mut interrupted = block_on(first.prompt_records(
        vec![user("sequential-crash-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start sequential crash prefix");
    loop {
        let event = block_on(interrupted.next())
            .expect("tool assistant commitment")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    let failing: Arc<dyn SessionStorage> = Arc::new(FailAfterToolResultStorage::new(inner.clone()));
    let executions = Arc::new(AtomicUsize::new(0));
    let mut recovery_tools = ToolRegistry::new();
    recovery_tools
        .register(Arc::new(EchoTool::with_counter(executions.clone())))
        .expect("unique recovery tool");
    let mut resumed = block_on(AgentHarness::open(
        session(failing, "sequential-crash-resume"),
        agent(std::iter::empty::<ScriptedResponse>(), recovery_tools),
    ))
    .expect("open crash recovery");
    let stream = block_on(resumed.resume_run(CancellationToken::new()))
        .expect("recovery remains lazy until the stream is polled");
    let events = block_on(stream.collect::<Vec<_>>());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(HarnessError::Session(_))))
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let state = inner.state_snapshot().expect("crash-prefix state");
    let starts = state
        .records_in_sequence_order()
        .iter()
        .filter_map(|record| match record {
            OperationRecord::ToolStarted { tool_index, .. } => Some(*tool_index),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, [0]);
    assert_eq!(
        state
            .entries_in_sequence_order()
            .iter()
            .filter(|entry| matches!(
                entry,
                pi_agent_session::SessionEntry::Message {
                    message: AgentRecord::Llm(Message::ToolResult(_)),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn harness_open_rejects_multiple_operations_as_corruption() {
    // Architecture v2 part 2 §7.6 and §10.10
    // session_multiple_open_operations_is_corruption. Pi basis:
    // harness/session/state.ts rejects multiple open operations.
    let state = SessionState::replay([
        operation_started(Sequence::new(1), "one", empty_intent()),
        operation_started(Sequence::new(2), "two", empty_intent()),
    ])
    .expect("reducer permits observing corrupt imported history");
    let storage: Arc<dyn SessionStorage> = Arc::new(ReadOnlyStorage { state });
    let result = block_on(AgentHarness::open(
        session(storage, "corrupt"),
        agent([text_response("unused")], ToolRegistry::new()),
    ));
    let Err(error) = result else {
        panic!("multiple open operations must reject harness open");
    };
    assert!(matches!(
        error,
        HarnessError::CorruptOpenOperations { open_operations } if open_operations.len() == 2
    ));
}

#[test]
fn harness_open_rejects_pinned_reducer_record_corruption() {
    // Architecture v2 part 2 §7.6. Pi basis:
    // packages/agent/test/harness/reducer.test.ts corruptionCases.
    let queue_target = ProvisionedEntry::Message {
        id: EntryId::new("queued-after-abort"),
        message: user("queued-after-abort-message", "late"),
        terminate: false,
    };
    let cases = [
        (
            RecoveryCorruptionReason::UnknownOperation,
            vec![SessionMutation::Record {
                record: OperationRecord::AbortRequested {
                    base: test_record_base(1, "unknown-abort"),
                    run_id: RunId::new("missing"),
                },
            }],
        ),
        (
            RecoveryCorruptionReason::RecordAfterFinish,
            vec![
                operation_started(Sequence::new(1), "run", empty_intent()),
                SessionMutation::Record {
                    record: OperationRecord::Finished {
                        base: test_record_base(2, "finish"),
                        run_id: RunId::new("run"),
                        outcome: OperationOutcome::Completed,
                        error: None,
                    },
                },
                SessionMutation::Record {
                    record: OperationRecord::AbortRequested {
                        base: test_record_base(3, "late-abort"),
                        run_id: RunId::new("run"),
                    },
                },
            ],
        ),
        (
            RecoveryCorruptionReason::NonConsecutiveAttempt,
            vec![
                operation_started(Sequence::new(1), "run", empty_intent()),
                SessionMutation::Record {
                    record: OperationRecord::StepAttempt {
                        base: test_record_base(2, "attempt-two"),
                        run_id: RunId::new("run"),
                        step: OperationStep::Assistant,
                        attempt: 2,
                        result_entry_id: EntryId::new("assistant"),
                        compaction_reason: None,
                    },
                },
            ],
        ),
        (
            RecoveryCorruptionReason::QueueAfterAbort,
            vec![
                operation_started(Sequence::new(1), "run", empty_intent()),
                SessionMutation::Record {
                    record: OperationRecord::AbortRequested {
                        base: test_record_base(2, "abort"),
                        run_id: RunId::new("run"),
                    },
                },
                SessionMutation::Record {
                    record: OperationRecord::QueueEnqueued {
                        base: test_record_base(3, "late-queue"),
                        run_id: Some(RunId::new("run")),
                        queue: QueueKind::Steer,
                        target: queue_target,
                    },
                },
            ],
        ),
    ];

    for (reason, mutations) in cases {
        let state = SessionState::replay(mutations).expect("permissive storage reducer");
        let storage: Arc<dyn SessionStorage> = Arc::new(ReadOnlyStorage { state });
        let result = block_on(AgentHarness::open(
            session(storage, "invalid-records"),
            agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
        ));
        assert!(matches!(
            result,
            Err(HarnessError::CorruptRecordLog {
                reason: actual,
                ..
            }) if actual == reason
        ));
    }
}

#[test]
fn harness_open_rejects_provisioned_entry_payload_mismatch() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.test.ts corruption case
    // "a provisioned id exists with different content" and reducer.ts
    // validateExactProvisionedEntry.
    let target = ProvisionedEntry::Message {
        id: EntryId::new("prompt"),
        message: user("expected-message", "expected"),
        terminate: false,
    };
    let state = SessionState::replay([
        operation_started(
            Sequence::new(1),
            "run",
            OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: vec![target],
                system_prompt_override: None,
                resume_data: BTreeMap::new(),
            },
        ),
        SessionMutation::Entry {
            lane: Some(LaneName::new("main")),
            entry: SessionEntry::Message {
                base: test_entry_base(2, "prompt"),
                message: user("different-message", "different"),
                terminate: false,
            },
        },
    ])
    .expect("permissive storage reducer");
    let storage: Arc<dyn SessionStorage> = Arc::new(ReadOnlyStorage { state });
    let result = block_on(AgentHarness::open(
        session(storage, "mismatched-provision"),
        agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
    ));
    assert!(matches!(
        result,
        Err(HarnessError::CorruptRecordLog {
            reason: RecoveryCorruptionReason::ProvisionedEntryMismatch,
            ..
        })
    ));
}

#[test]
fn harness_open_rejects_step_attempt_result_entry_type_mismatch() {
    // Architecture v2 part 2 §7.6. Pi basis: reducer.ts
    // validateAttemptResult and reducer.test.ts provisioned result corruption
    // coverage. An assistant attempt may close only with an assistant entry.
    let state = SessionState::replay([
        operation_started(Sequence::new(1), "run", empty_intent()),
        SessionMutation::Record {
            record: OperationRecord::StepAttempt {
                base: test_record_base(2, "assistant-attempt"),
                run_id: RunId::new("run"),
                step: OperationStep::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("wrong-result"),
                compaction_reason: None,
            },
        },
        SessionMutation::Entry {
            lane: Some(LaneName::new("main")),
            entry: SessionEntry::Custom {
                base: test_entry_base(3, "wrong-result"),
                custom_type: "not-an-assistant".to_owned(),
                data: None,
            },
        },
    ])
    .expect("permissive storage reducer");
    let storage: Arc<dyn SessionStorage> = Arc::new(ReadOnlyStorage { state });
    let result = block_on(AgentHarness::open(
        session(storage, "mismatched-attempt-result"),
        agent(std::iter::empty::<ScriptedResponse>(), ToolRegistry::new()),
    ));
    assert!(matches!(
        result,
        Err(HarnessError::CorruptRecordLog {
            reason: RecoveryCorruptionReason::ProvisionedEntryMismatch,
            ..
        })
    ));
}

#[test]
fn harness_late_ingress_is_cancelled_before_operation_finish() {
    // Architecture v2 part 2 §8.4. Pi basis: reducer.test.ts rejects both
    // record_after_finish and invalid queue-cancellation prefixes. This gate
    // pauses after durable enqueue so RunFinished deterministically races the
    // second active-run check.
    let inner = storage("late-ingress");
    let (pausing, appended, release) = PausingQueueStorage::new(inner.clone());
    let durable: Arc<dyn SessionStorage> = pausing;
    let mut harness = block_on(AgentHarness::open(
        session(durable, "late-ingress"),
        agent([text_response("done")], ToolRegistry::new()),
    ))
    .expect("open harness");
    let control = harness.control();
    let mut stream =
        block_on(harness.prompt_records(vec![user("late-user", "go")], CancellationToken::new()))
            .expect("start run");
    loop {
        let event = block_on(stream.next())
            .expect("assistant commit")
            .expect("valid event");
        if matches!(
            event,
            HarnessRunEvent::Agent(event)
                if matches!(event.as_ref(), AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                })
        ) {
            break;
        }
    }

    let (ingress, remaining, ()) = block_on(async {
        futures_util::future::join3(
            control.steer(user("late-steer", "too late")),
            stream.collect::<Vec<_>>(),
            async move {
                appended.await.expect("queue append signal");
                release.send(()).expect("release queue append");
            },
        )
        .await
    });
    assert!(matches!(
        ingress,
        Err(HarnessError::OperationChanged { .. })
    ));
    assert!(remaining.iter().all(Result::is_ok));
    let state = inner.state_snapshot().expect("race state");
    let records = state.records_in_sequence_order();
    let enqueue = records
        .iter()
        .position(|record| matches!(record, OperationRecord::QueueEnqueued { .. }))
        .expect("durable enqueue");
    let cancelled = records
        .iter()
        .position(|record| matches!(record, OperationRecord::QueueCancelled { .. }))
        .expect("compensating cancellation");
    let finished = records
        .iter()
        .position(|record| matches!(record, OperationRecord::Finished { .. }))
        .expect("operation terminal");
    assert!(enqueue < cancelled && cancelled < finished);
}

#[test]
fn harness_abort_and_ingress_are_serialized_without_queue_after_abort() {
    // Architecture v2 part 2 §7.6 and §8.4. Pi basis: reducer.test.ts
    // `queue_after_abort`. `join` polls cancel first, while the shared
    // transition gate makes the competing enqueue observe the durable abort.
    let storage = storage("abort-ingress-race");
    let mut harness = block_on(AgentHarness::open(
        session(storage.clone(), "abort-ingress"),
        agent([text_response("unused")], ToolRegistry::new()),
    ))
    .expect("open harness");
    let control = harness.control();
    let mut stream =
        block_on(harness.prompt_records(vec![user("abort-user", "go")], CancellationToken::new()))
            .expect("start run");
    assert!(matches!(
        block_on(stream.next()),
        Some(Ok(HarnessRunEvent::Harness(HarnessEvent::RunStart { .. })))
    ));
    let (cancelled, ingress) = block_on(futures_util::future::join(
        control.cancel(),
        control.steer(user("after-abort", "late")),
    ));
    assert!(cancelled.is_ok());
    assert!(matches!(ingress, Err(HarnessError::Session(_))));
    let remaining = block_on(stream.collect::<Vec<_>>());
    assert!(remaining.iter().all(Result::is_ok));
    let state = storage.state_snapshot().expect("abort race state");
    let records = state.records_in_sequence_order();
    let abort = records
        .iter()
        .position(|record| matches!(record, OperationRecord::AbortRequested { .. }))
        .expect("abort request");
    assert!(!records.iter().skip(abort + 1).any(|record| matches!(
        record,
        OperationRecord::QueueEnqueued {
            queue: QueueKind::Steer | QueueKind::FollowUp,
            ..
        }
    )));
}

#[test]
fn harness_events_deliver_matching_listeners_and_watchers() {
    // Pi basis: packages/agent/test/harness/events.test.ts, "delivers matching events".
    let bus = HarnessEventBus::new();
    let direct = Arc::new(Mutex::new(Vec::new()));
    let watched = Arc::new(Mutex::new(Vec::new()));
    let direct_events = direct.clone();
    let mut subscription = bus.on(HarnessEventType::RunStart, move |event| {
        lock(&direct_events).push(event.clone());
    });
    let watch = bus.watch(|| ());
    let watched_events = watched.clone();
    watch.start(move |event| lock(&watched_events).push(event.clone()));
    let start = run_start();
    let end = run_end();

    bus.emit(start.clone());
    bus.emit(end.clone());
    subscription.unsubscribe();
    bus.emit(start.clone());

    assert_eq!(lock(&direct).as_slice(), std::slice::from_ref(&start));
    assert_eq!(lock(&watched).as_slice(), [start.clone(), end, start]);
    assert_eq!(
        serde_json::to_value(run_end()).expect("serialize Pi event"),
        json!({
            "type": "run_end",
            "lane": "main",
            "runId": "run-1",
            "outcome": "completed",
            "leafId": "entry-1",
        })
    );
}

#[test]
fn harness_event_watch_has_no_snapshot_gap() {
    // Pi basis: packages/agent/test/harness/events.test.ts, "captures a snapshot
    // without an event gap".
    let bus = HarnessEventBus::new();
    let watch = bus.watch(|| {
        bus.emit(run_start());
        "snapshot"
    });
    let received = Arc::new(Mutex::new(Vec::new()));
    let listener_events = received.clone();
    watch.start(move |event| lock(&listener_events).push(event.clone()));
    bus.emit(run_end());

    assert_eq!(watch.snapshot, "snapshot");
    assert_eq!(lock(&received).as_slice(), [run_start(), run_end()]);
}

#[test]
fn harness_local_family_accepts_rc_state_and_matches_lifecycle() {
    // Architecture v2 part 2 §9.2. Pi basis: the same harness event and durable
    // reducer contracts, exposed without Send bounds for local executors.
    let storage = Rc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "local",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .expect("valid local storage"),
    );
    let mut harness = block_on(LocalAgentHarness::open(
        local_session(storage.clone(), "local"),
        local_agent([text_response("done")]),
    ))
    .expect("open local harness");
    let observed = Rc::new(RefCell::new(Vec::new()));
    let listener_state = observed.clone();
    let _subscription = harness.events().on(HarnessEventType::RunEnd, move |event| {
        listener_state.borrow_mut().push(event.clone());
    });
    let events = block_on(async {
        harness
            .prompt_records(vec![user("local-user", "go")], CancellationToken::new())
            .await
            .expect("start local run")
            .collect::<Vec<_>>()
            .await
    });

    assert!(events.iter().all(Result::is_ok));
    assert_eq!(observed.borrow().len(), 1);
    assert!(matches!(
        block_on(LocalSessionStorage::load_state(storage.as_ref()))
            .expect("local state")
            .recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
}

#[test]
fn harness_local_tool_started_persists_final_authorized_arguments() {
    // Local/WASM counterpart of the durable §7.2 ToolStarted argument
    // boundary and §7.6 recovery. Non-idempotent preparation and authorization
    // prove that a resumed local invocation consumes the persisted authorized
    // intent without running current preflight again.
    let storage = Rc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "local-authorized",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .expect("valid local storage"),
    );
    let preparations = Arc::new(AtomicUsize::new(0));
    let authorizations = Arc::new(AtomicUsize::new(0));
    let first_observed = Rc::new(RefCell::new(Vec::new()));
    let mut first_tools = LocalToolRegistry::new();
    first_tools
        .register_with_argument_preparer(
            Rc::new(PausingLocalTool::new(first_observed.clone())),
            local_non_idempotent_preparer(preparations.clone()),
        )
        .expect("unique first local tool");
    let mut first_agent = LocalAgent::new(
        Rc::new(ScriptedRuntime::new([tool_call_response(
            "echo",
            json!({"value":"raw"}),
        )])),
        agent_state(),
        first_tools,
    )
    .expect("valid local agent");
    first_agent
        .set_tool_policy(Rc::new(NonIdempotentToolPolicy {
            authorizations: authorizations.clone(),
        }))
        .expect("idle local policy replacement");
    let mut first = block_on(LocalAgentHarness::open(
        local_session(storage.clone(), "local-authorized-first"),
        first_agent,
    ))
    .expect("open local authorized harness");
    first.set_tool_replay_policy("echo", ToolReplayPolicy::Safe);
    let mut interrupted = block_on(first.prompt_records(
        vec![user("local-authorized-user", "go")],
        CancellationToken::new(),
    ))
    .expect("start local authorized run");
    loop {
        let event = block_on(interrupted.next())
            .expect("local tool execution update")
            .expect("valid local event");
        if matches!(event, HarnessRunEvent::Agent(event)
            if matches!(event.as_ref(), AgentEvent::ToolExecutionUpdated { .. }))
        {
            break;
        }
    }
    drop(interrupted);
    drop(first);

    assert_eq!(
        first_observed.borrow().as_slice(),
        [prepared_recovery_arguments()]
    );
    let state = storage.state_snapshot().expect("local authorized state");
    assert!(state.records_in_sequence_order().iter().any(|record| {
        matches!(
            record,
            OperationRecord::ToolStarted {
                effective_args,
                replay: ToolReplayPolicy::Safe,
                ..
            } if effective_args == &prepared_recovery_arguments()
        )
    }));

    let replay_observed = Rc::new(RefCell::new(Vec::new()));
    let mut replay_tools = LocalToolRegistry::new();
    replay_tools
        .register_with_argument_preparer(
            Rc::new(RecordingLocalTool::new(replay_observed.clone())),
            local_non_idempotent_preparer(preparations.clone()),
        )
        .expect("unique replay local tool");
    let mut replay_agent = LocalAgent::new(
        Rc::new(ScriptedRuntime::new([text_response("after local replay")])),
        agent_state(),
        replay_tools,
    )
    .expect("valid replay local agent");
    replay_agent
        .set_tool_policy(Rc::new(NonIdempotentToolPolicy {
            authorizations: authorizations.clone(),
        }))
        .expect("idle local replay policy replacement");
    let mut resumed = block_on(LocalAgentHarness::open(
        local_session(storage, "local-authorized-resume"),
        replay_agent,
    ))
    .expect("open local replay harness");
    let events = block_on(async {
        resumed
            .resume_run(CancellationToken::new())
            .await
            .expect("resume local authorized invocation")
            .collect::<Vec<_>>()
            .await
    });
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        replay_observed.borrow().as_slice(),
        [prepared_recovery_arguments()]
    );
    assert_eq!(preparations.load(Ordering::SeqCst), 1);
    assert_eq!(authorizations.load(Ordering::SeqCst), 1);
}

#[test]
fn resource_formatting_matches_pinned_pi() {
    // Pi basis: packages/agent/test/harness/resource-formatting.test.ts.
    let skill = LoadedSkill::new(
        SkillDescriptor {
            id: SkillId::new("inspect"),
            name: "inspect".to_owned(),
            description: "Inspect things".to_owned(),
            location: "/project/.pi/skills/inspect/SKILL.md".to_owned(),
            disable_model_invocation: false,
        },
        vec![PromptFragment {
            name: None,
            content: "Use inspection tools.".to_owned(),
        }],
        Vec::new(),
    )
    .expect("valid skill");
    assert_eq!(
        format_skill_invocation(&skill, Some("Check errors.")),
        "<skill name=\"inspect\" location=\"/project/.pi/skills/inspect/SKILL.md\">\nReferences are relative to /project/.pi/skills/inspect.\n\nUse inspection tools.\n</skill>\n\nCheck errors."
    );

    let template = PromptTemplate::new("review", None, None, "Review $1 with $ARGUMENTS")
        .expect("valid template");
    let registry = StaticPromptTemplateRegistry::new([template]).expect("unique template");
    assert_eq!(
        registry
            .resolve("review", &TemplateArguments::new(["a.ts", "care"]))
            .expect("render template")
            .content,
        "Review a.ts with a.ts care"
    );
}

#[test]
fn harness_bash_execution_message_formats_and_projects_per_pi() {
    // Pi basis: packages/agent/src/harness/messages.ts bashExecutionToText and
    // convertToLlm; resource formatting remains a pure harness concern.
    let message = BashExecutionMessage {
        schema_version: BASH_EXECUTION_MESSAGE_SCHEMA_VERSION,
        command: "cargo test".to_owned(),
        output: "failed".to_owned(),
        exit_code: Some(1),
        cancelled: false,
        truncated: true,
        full_output_path: Some("/tmp/full.log".to_owned()),
        timestamp: Timestamp::from_unix_millis(1),
        exclude_from_context: false,
    };
    let expected = "Ran `cargo test`\n```\nfailed\n```\n\nCommand exited with code 1\n\n[Output truncated. Full output: /tmp/full.log]";
    assert_eq!(format_bash_execution(&message), expected);
    let records = [bash_execution_record(&message).expect("serialize custom message")];
    let projected = convert_harness_records_to_llm(&records);
    assert_eq!(projected.len(), 1);
    assert_eq!(projected_message_text(&projected[0]), expected);
}

fn empty_intent() -> OperationIntent {
    OperationIntent::Run {
        original_prompt: Vec::new(),
        initial_messages: Vec::new(),
        system_prompt_override: None,
        resume_data: Default::default(),
    }
}

fn operation_started(sequence: Sequence, id: &str, intent: OperationIntent) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Started {
            base: OperationRecordBase {
                id: OperationRecordId::new(id),
                sequence,
                lane: LaneName::new("main"),
                timestamp: Timestamp::from_unix_millis(
                    i64::try_from(sequence.get()).expect("small test sequence"),
                ),
            },
            source_leaf_id: None,
            intent,
        },
    }
}

fn test_record_base(sequence: u64, id: &str) -> OperationRecordBase {
    OperationRecordBase {
        id: OperationRecordId::new(id),
        sequence: Sequence::new(sequence),
        lane: LaneName::new("main"),
        timestamp: Timestamp::from_unix_millis(i64::try_from(sequence).expect("small sequence")),
    }
}

fn test_entry_base(sequence: u64, id: &str) -> EntryBase {
    EntryBase {
        id: EntryId::new(id),
        sequence: Sequence::new(sequence),
        parent_id: None,
        timestamp: Timestamp::from_unix_millis(i64::try_from(sequence).expect("small sequence")),
    }
}

fn run_start() -> HarnessEvent {
    HarnessEvent::RunStart {
        lane: LaneName::new("main"),
        run_id: RunId::new("run-1"),
    }
}

fn run_end() -> HarnessEvent {
    HarnessEvent::RunEnd {
        lane: LaneName::new("main"),
        run_id: RunId::new("run-1"),
        outcome: HarnessRunOutcome::Completed,
        leaf_id: EntryId::new("entry-1"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ReadOnlyStorage {
    state: SessionState,
}

struct PausingQueueStorage {
    inner: Arc<InMemorySessionStorage>,
    appended: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

struct FailAfterToolResultStorage {
    inner: Arc<InMemorySessionStorage>,
    failed: Mutex<bool>,
}

impl FailAfterToolResultStorage {
    fn new(inner: Arc<InMemorySessionStorage>) -> Self {
        Self {
            inner,
            failed: Mutex::new(false),
        }
    }
}

impl PausingQueueStorage {
    fn new(
        inner: Arc<InMemorySessionStorage>,
    ) -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (appended_tx, appended_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        (
            Arc::new(Self {
                inner,
                appended: Mutex::new(Some(appended_tx)),
                release: Mutex::new(Some(release_rx)),
            }),
            appended_rx,
            release_tx,
        )
    }

    fn new_local(
        inner: Arc<InMemorySessionStorage>,
    ) -> (Rc<Self>, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (appended_tx, appended_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        (
            Rc::new(Self {
                inner,
                appended: Mutex::new(Some(appended_tx)),
                release: Mutex::new(Some(release_rx)),
            }),
            appended_rx,
            release_tx,
        )
    }
}

impl SessionStorage for PausingQueueStorage {
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        SessionStorage::metadata(self.inner.as_ref())
    }

    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>> {
        SessionStorage::load_state(self.inner.as_ref())
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move {
            let pause = mutations.iter().any(|mutation| {
                matches!(
                    mutation,
                    SessionMutation::Record {
                        record: OperationRecord::QueueEnqueued { .. }
                    }
                )
            });
            let receipt =
                SessionStorage::append(self.inner.as_ref(), expected_sequence, mutations).await?;
            if pause {
                if let Some(sender) = lock(&self.appended).take() {
                    let _ = sender.send(());
                }
                let release = lock(&self.release).take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            }
            Ok(receipt)
        })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        SessionStorage::log(self.inner.as_ref(), after, limit)
    }

    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        SessionStorage::repair_tail(self.inner.as_ref())
    }
}

impl LocalSessionStorage for PausingQueueStorage {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        LocalSessionStorage::metadata(self.inner.as_ref())
    }

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>> {
        LocalSessionStorage::load_state(self.inner.as_ref())
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move {
            let pause = mutations.iter().any(|mutation| {
                matches!(
                    mutation,
                    SessionMutation::Record {
                        record: OperationRecord::QueueEnqueued { .. }
                    }
                )
            });
            let receipt =
                LocalSessionStorage::append(self.inner.as_ref(), expected_sequence, mutations)
                    .await?;
            if pause {
                if let Some(sender) = lock(&self.appended).take() {
                    let _ = sender.send(());
                }
                let release = lock(&self.release).take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            }
            Ok(receipt)
        })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        LocalSessionStorage::log(self.inner.as_ref(), after, limit)
    }

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        LocalSessionStorage::repair_tail(self.inner.as_ref())
    }
}

impl SessionStorage for FailAfterToolResultStorage {
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        SessionStorage::metadata(self.inner.as_ref())
    }

    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>> {
        SessionStorage::load_state(self.inner.as_ref())
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move {
            let commits_tool_result = mutations.iter().any(|mutation| {
                matches!(
                    mutation,
                    SessionMutation::Entry {
                        entry: pi_agent_session::SessionEntry::Message {
                            message: AgentRecord::Llm(Message::ToolResult(_)),
                            ..
                        },
                        ..
                    }
                )
            });
            let receipt =
                SessionStorage::append(self.inner.as_ref(), expected_sequence, mutations).await?;
            if commits_tool_result {
                let mut failed = lock(&self.failed);
                if !*failed {
                    *failed = true;
                    return Err(SessionError::new(
                        SessionErrorKind::Storage,
                        "simulated crash after first recovered result commit",
                    ));
                }
            }
            Ok(receipt)
        })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        SessionStorage::log(self.inner.as_ref(), after, limit)
    }

    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        SessionStorage::repair_tail(self.inner.as_ref())
    }
}

impl SessionStorage for ReadOnlyStorage {
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move {
            Ok(SessionMetadata {
                schema_version: SESSION_METADATA_SCHEMA_VERSION,
                session_id: SessionId::new("read-only"),
                created_at: Timestamp::from_unix_millis(1),
                parent_session_id: None,
                environment: SessionEnvironmentMetadata::default(),
                last_sequence: self.state.sequence(),
            })
        })
    }

    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { Ok(self.state.clone()) })
    }

    fn append(
        &self,
        _expected_sequence: Sequence,
        _mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async {
            Err(SessionError::new(
                SessionErrorKind::Storage,
                "read-only test storage",
            ))
        })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move {
            Ok(self
                .state
                .log()
                .iter()
                .filter(|mutation| after.is_none_or(|sequence| mutation.sequence() > sequence))
                .take(limit.unwrap_or(usize::MAX))
                .cloned()
                .collect())
        })
    }

    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move {
            Ok(TailRepairReport {
                schema_version: TAIL_REPAIR_REPORT_SCHEMA_VERSION,
                repaired: false,
                removed_bytes: 0,
                last_sequence: self.state.sequence(),
            })
        })
    }
}
