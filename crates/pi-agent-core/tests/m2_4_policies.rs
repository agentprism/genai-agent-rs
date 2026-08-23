use futures_executor::block_on;
use futures_util::StreamExt;
use pi_agent_core::*;
use pi_ai::*;
use serde_json::{json, value::to_raw_value};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn state() -> AgentState {
    AgentState::new(
        "You are helpful.",
        ModelRef::new("scripted", "model-a"),
        ReasoningLevel::Off,
    )
}

fn user(id: &str, text: &str) -> AgentRecord {
    AgentRecord::Llm(Message::User(UserMessage {
        id: MessageId::new(id),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-text")),
            text: text.into(),
        }],
        timestamp: Timestamp::default(),
    }))
}

fn text_of_message(message: &Message) -> Option<&str> {
    let Message::User(message) = message else {
        return None;
    };
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.as_str()),
        ContentBlock::Image { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::ToolCall { .. } => None,
    })
}

fn text_of_record(record: &AgentRecord) -> Option<&str> {
    let AgentRecord::Llm(message) = record else {
        return None;
    };
    text_of_message(message)
}

fn collect(stream: SendBoxStream<'_, AgentEvent>) -> Vec<AgentEvent> {
    block_on(stream.collect())
}

fn collect_local(stream: LocalBoxStream<'_, AgentEvent>) -> Vec<AgentEvent> {
    block_on(stream.collect())
}

#[derive(Clone)]
struct RecordingRuntime {
    inner: ScriptedRuntime,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelRuntime for RecordingRuntime {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>> {
        Box::pin(async move {
            lock(&self.requests).push(request.clone());
            ModelRuntime::stream(&self.inner, request, cancellation).await
        })
    }
}

struct TransformingPolicy {
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl ContextPolicy for TransformingPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        _state: AgentStateView<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            lock(&self.order).push("context_policy");
            Ok(PreparedAgentRecords {
                records: vec![user("transformed", "transformed")],
                model_override: None,
                options_override: None,
                report: None,
            })
        })
    }
}

struct ObservingProjector {
    order: Arc<Mutex<Vec<&'static str>>>,
    calls: AtomicUsize,
}

impl MessageProjector for ObservingProjector {
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> SendBoxFuture<'a, Result<Vec<Message>, ContextError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            lock(&self.order).push("message_projector");
            Ok(records
                .iter()
                .filter_map(|record| match record {
                    AgentRecord::Llm(message) => Some(message.clone()),
                    AgentRecord::Custom { .. } => None,
                })
                .collect())
        })
    }
}

#[test]
fn agent_transform_context_runs_before_projector() {
    // §10.9 Context phases. Pi basis: packages/agent/src/agent-loop.ts
    // streamAssistantResponse applies transformContext before convertToLlm.
    let order = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([text_response("done")]),
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    agent
        .set_context_policy(Arc::new(TransformingPolicy {
            order: order.clone(),
        }))
        .unwrap();
    agent
        .set_message_projector(Arc::new(ObservingProjector {
            order: order.clone(),
            calls: AtomicUsize::new(0),
        }))
        .unwrap();

    let events =
        collect(agent.prompt_records([user("prompt", "original")], CancellationToken::new()));

    assert_eq!(
        lock(&order).as_slice(),
        ["context_policy", "message_projector"]
    );
    let requests = lock(&requests);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .context
            .messages
            .iter()
            .filter_map(text_of_message)
            .collect::<Vec<_>>(),
        ["transformed"]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ContextPrepared { target, report, .. }
            if target == &ModelRef::new("scripted", "model-a")
                && report.target
                    == ModelFingerprint::new("scripted", "provider-neutral", "model-a")
    )));
}

struct EnqueueOnceTurnPolicy {
    control: AgentControl,
    enqueued: AtomicBool,
    next: NextTurn,
}

impl TurnPolicy for EnqueueOnceTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            if !self.enqueued.swap(true, Ordering::AcqRel) {
                self.control
                    .steer(user("steering", "continue"))
                    .await
                    .map_err(|error| TurnPolicyError::new(error.to_string()))?;
                return Ok(self.next.clone());
            }
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

struct CountingProjector {
    calls: Arc<AtomicUsize>,
}

impl MessageProjector for CountingProjector {
    fn project<'a>(
        &'a self,
        records: &'a [AgentRecord],
    ) -> SendBoxFuture<'a, Result<Vec<Message>, ContextError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            <DefaultMessageProjector as MessageProjector>::project(
                &DefaultMessageProjector,
                records,
            )
            .await
        })
    }
}

#[test]
fn agent_projector_runs_once_per_model_turn() {
    // §10.9 Context phases. Pi basis: agent-loop.ts calls convertToLlm once in
    // streamAssistantResponse, which itself runs once for each provider turn.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([
            text_response("first"),
            text_response("second"),
        ])),
        state(),
        ToolRegistry::new(),
    )
    .unwrap();
    agent
        .set_message_projector(Arc::new(CountingProjector {
            calls: calls.clone(),
        }))
        .unwrap();
    agent
        .set_turn_policy(Arc::new(EnqueueOnceTurnPolicy {
            control: agent.control(),
            enqueued: AtomicBool::new(false),
            next: NextTurn::default(),
        }))
        .unwrap();

    collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));

    assert_eq!(calls.load(Ordering::Acquire), 2);
}

struct CancellationObservingPolicy {
    observed: Arc<AtomicBool>,
}

impl ContextPolicy for CancellationObservingPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        _state: AgentStateView<'a>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            self.observed
                .store(cancellation.is_cancelled(), Ordering::Release);
            Err(ContextError::Cancelled)
        })
    }
}

#[test]
fn agent_context_policy_receives_cancellation() {
    // §10.9 Context phases. Pi basis: transformContext receives the active
    // AbortSignal in agent-loop.ts and agent.ts forwards the run signal.
    let observed = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([])),
        state(),
        ToolRegistry::new(),
    )
    .unwrap();
    agent
        .set_context_policy(Arc::new(CancellationObservingPolicy {
            observed: observed.clone(),
        }))
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let events = collect(agent.prompt_records([user("prompt", "start")], cancellation));

    assert!(observed.load(Ordering::Acquire));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            outcome: RunOutcome::Cancelled { .. }
        })
    ));
}

struct CountingTurnPolicy {
    prepare_calls: Arc<AtomicUsize>,
}

struct MarkerTool {
    spec: ToolSpec,
}

struct LocalMarkerTool {
    spec: ToolSpec,
}

impl LocalMarkerTool {
    fn new(name: &str) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: name.into(),
                description: format!("{name} local marker tool"),
                parameters: json!({"type": "object", "additionalProperties": false}),
                constrained_sampling: None,
            },
        }
    }
}

impl LocalTool for LocalMarkerTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute(
        &self,
        _context: ToolCallContext,
        _updates: Rc<dyn LocalToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::new(Vec::new())) })
    }
}

impl MarkerTool {
    fn new(name: &str) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: name.into(),
                description: format!("{name} marker tool"),
                parameters: json!({"type": "object", "additionalProperties": false}),
                constrained_sampling: None,
            },
        }
    }
}

impl Tool for MarkerTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute(
        &self,
        _context: ToolCallContext,
        _updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::new(Vec::new())) })
    }
}

fn tool_registry(name: &str) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MarkerTool::new(name))).unwrap();
    tools
}

fn local_tool_registry(name: &str) -> LocalToolRegistry {
    let mut tools = LocalToolRegistry::new();
    tools.register(Rc::new(LocalMarkerTool::new(name))).unwrap();
    tools
}

impl TurnPolicy for CountingTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            self.prepare_calls.fetch_add(1, Ordering::AcqRel);
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

#[test]
fn agent_prepare_next_turn_runs_after_turn_finished() {
    // §10.9 Context phases. Pi basis: agent-loop.ts emits turn_end before it
    // awaits prepareNextTurn.
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([text_response("done")])),
        state(),
        ToolRegistry::new(),
    )
    .unwrap();
    agent
        .set_turn_policy(Arc::new(CountingTurnPolicy {
            prepare_calls: prepare_calls.clone(),
        }))
        .unwrap();
    let mut stream = agent.prompt_records([user("prompt", "start")], CancellationToken::new());

    loop {
        let event = block_on(stream.next()).expect("run must reach TurnFinished");
        if matches!(event, AgentEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert_eq!(prepare_calls.load(Ordering::Acquire), 0);

    block_on(stream.collect::<Vec<_>>());
    assert_eq!(prepare_calls.load(Ordering::Acquire), 1);
}

fn run_with_next_turn(next: NextTurn) -> Vec<ModelRequest> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([text_response("first"), text_response("second")]),
        requests: requests.clone(),
    };
    let mut initial_state = state();
    initial_state.system_prompt = "first prompt".into();
    let mut agent = Agent::new(
        Arc::new(runtime),
        initial_state,
        tool_registry("first-tool"),
    )
    .unwrap();
    agent
        .set_turn_policy(Arc::new(EnqueueOnceTurnPolicy {
            control: agent.control(),
            enqueued: AtomicBool::new(false),
            next,
        }))
        .unwrap();
    collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));
    lock(&requests).clone()
}

#[derive(Debug, Eq, PartialEq)]
struct ToolContextSnapshot {
    phase: &'static str,
    system_prompt: String,
    user_texts: Vec<String>,
    record_roles: Vec<&'static str>,
    has_first_tool: bool,
    has_second_tool: bool,
}

fn snapshot_tool_context(phase: &'static str, context: &AgentContext) -> ToolContextSnapshot {
    ToolContextSnapshot {
        phase,
        system_prompt: context.system_prompt.clone(),
        user_texts: context
            .records
            .iter()
            .filter_map(text_of_record)
            .map(str::to_owned)
            .collect(),
        record_roles: context.records.iter().map(record_role_name).collect(),
        has_first_tool: context.tools.get("first-tool").is_some(),
        has_second_tool: context.tools.get("second-tool").is_some(),
    }
}

struct ObservingToolContextPolicy {
    snapshots: Arc<Mutex<Vec<ToolContextSnapshot>>>,
}

impl ToolPolicy for ObservingToolContextPolicy {
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async move {
            lock(&self.snapshots).push(snapshot_tool_context("authorize", context.context));
            Ok(ToolAuthorization::Allow)
        })
    }

    fn finalize<'a>(
        &'a self,
        context: AfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        Box::pin(async move {
            lock(&self.snapshots).push(snapshot_tool_context("finalize", context.context));
            Ok(ToolOutputPatch {
                terminate: Some(true),
                ..ToolOutputPatch::default()
            })
        })
    }
}

#[test]
fn agent_prepare_next_turn_can_replace_context() {
    // §10.9 Context phases. Pi basis: agent-loop.test.ts "should use
    // prepareNextTurn snapshot before continuing"; types.ts:97 and
    // agent-loop.ts prepare/finalize hooks receive that complete current
    // AgentContext, including its replaced system prompt and executable tools.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([
            text_response("first"),
            tool_call_response("second-tool", json!({})),
        ]),
        requests: requests.clone(),
    };
    let mut initial_state = state();
    initial_state.system_prompt = "first prompt".into();
    let mut agent = Agent::new(
        Arc::new(runtime),
        initial_state,
        tool_registry("first-tool"),
    )
    .unwrap();
    agent
        .set_turn_policy(Arc::new(EnqueueOnceTurnPolicy {
            control: agent.control(),
            enqueued: AtomicBool::new(false),
            next: NextTurn {
                context: Some(AgentRunContext {
                    system_prompt: "second prompt".into(),
                    records: vec![user("replacement", "replacement context")],
                    tools: tool_registry("second-tool"),
                }),
                model: None,
                reasoning: None,
            },
        }))
        .unwrap();
    agent
        .set_tool_policy(Arc::new(ObservingToolContextPolicy {
            snapshots: snapshots.clone(),
        }))
        .unwrap();

    collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));

    let requests = lock(&requests);

    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].context.system_prompt.as_deref(),
        Some("first prompt")
    );
    assert_eq!(
        requests[1].context.system_prompt.as_deref(),
        Some("second prompt")
    );
    assert_eq!(
        requests[1]
            .context
            .messages
            .iter()
            .filter_map(text_of_message)
            .collect::<Vec<_>>(),
        ["replacement context", "continue"]
    );
    assert_eq!(
        requests[0]
            .context
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["first-tool"]
    );
    assert_eq!(
        requests[1]
            .context
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["second-tool"]
    );
    assert_eq!(
        lock(&snapshots).as_slice(),
        [
            ToolContextSnapshot {
                phase: "authorize",
                system_prompt: "second prompt".into(),
                user_texts: vec!["replacement context".into(), "continue".into()],
                record_roles: vec!["user", "user", "assistant"],
                has_first_tool: false,
                has_second_tool: true,
            },
            ToolContextSnapshot {
                phase: "finalize",
                system_prompt: "second prompt".into(),
                user_texts: vec!["replacement context".into(), "continue".into()],
                record_roles: vec!["user", "user", "assistant"],
                has_first_tool: false,
                has_second_tool: true,
            },
        ]
    );
}

fn snapshot_local_tool_context(
    phase: &'static str,
    context: &LocalAgentContext,
) -> ToolContextSnapshot {
    ToolContextSnapshot {
        phase,
        system_prompt: context.system_prompt.clone(),
        user_texts: context
            .records
            .iter()
            .filter_map(text_of_record)
            .map(str::to_owned)
            .collect(),
        record_roles: context.records.iter().map(record_role_name).collect(),
        has_first_tool: context.tools.get("first-tool").is_some(),
        has_second_tool: context.tools.get("second-tool").is_some(),
    }
}

struct LocalReplacingTurnPolicy {
    control: AgentControl,
    next: RefCell<Option<LocalNextTurn>>,
}

impl LocalTurnPolicy for LocalReplacingTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: LocalCompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<LocalNextTurn, TurnPolicyError>> {
        Box::pin(async move {
            let next = self.next.borrow_mut().take();
            if let Some(next) = next {
                self.control
                    .steer(user("local-steering", "continue"))
                    .await
                    .map_err(|error| TurnPolicyError::new(error.to_string()))?;
                return Ok(next);
            }
            Ok(LocalNextTurn::default())
        })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: LocalCompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async { Ok(false) })
    }
}

struct LocalObservingToolContextPolicy {
    snapshots: Rc<RefCell<Vec<ToolContextSnapshot>>>,
}

impl LocalToolPolicy for LocalObservingToolContextPolicy {
    fn authorize<'a>(
        &'a self,
        context: LocalBeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        Box::pin(async move {
            self.snapshots
                .borrow_mut()
                .push(snapshot_local_tool_context("authorize", context.context));
            Ok(ToolAuthorization::Allow)
        })
    }

    fn finalize<'a>(
        &'a self,
        context: LocalAfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        Box::pin(async move {
            self.snapshots
                .borrow_mut()
                .push(snapshot_local_tool_context("finalize", context.context));
            Ok(ToolOutputPatch {
                terminate: Some(true),
                ..ToolOutputPatch::default()
            })
        })
    }
}

#[test]
fn agent_local_tool_hooks_receive_replaced_complete_context() {
    // §10.9 Context phases regression for part 2 §9.2's Local family. Pi
    // basis: packages/agent/src/types.ts:97 and agent-loop.ts:575-690 pass the
    // complete current AgentContext to both hooks after replacement.
    let snapshots = Rc::new(RefCell::new(Vec::new()));
    let mut local_state = state();
    local_state.system_prompt = "first prompt".into();
    let mut agent = LocalAgent::new(
        Rc::new(ScriptedRuntime::new([
            text_response("first"),
            tool_call_response("second-tool", json!({})),
        ])),
        local_state,
        local_tool_registry("first-tool"),
    )
    .unwrap();
    agent
        .set_turn_policy(Rc::new(LocalReplacingTurnPolicy {
            control: agent.control(),
            next: RefCell::new(Some(LocalNextTurn {
                context: Some(AgentRunContext {
                    system_prompt: "second prompt".into(),
                    records: vec![user("local-replacement", "replacement context")],
                    tools: local_tool_registry("second-tool"),
                }),
                model: None,
                reasoning: None,
            })),
        }))
        .unwrap();
    agent
        .set_tool_policy(Rc::new(LocalObservingToolContextPolicy {
            snapshots: snapshots.clone(),
        }))
        .unwrap();

    collect_local(agent.prompt_records([user("local-prompt", "start")], CancellationToken::new()));

    assert_eq!(
        snapshots.borrow().as_slice(),
        [
            ToolContextSnapshot {
                phase: "authorize",
                system_prompt: "second prompt".into(),
                user_texts: vec!["replacement context".into(), "continue".into()],
                record_roles: vec!["user", "user", "assistant"],
                has_first_tool: false,
                has_second_tool: true,
            },
            ToolContextSnapshot {
                phase: "finalize",
                system_prompt: "second prompt".into(),
                user_texts: vec!["replacement context".into(), "continue".into()],
                record_roles: vec!["user", "user", "assistant"],
                has_first_tool: false,
                has_second_tool: true,
            },
        ]
    );
}

#[derive(Debug, Eq, PartialEq)]
struct CompletedTurnSnapshot {
    phase: &'static str,
    context_user_texts: Vec<String>,
    new_message_roles: Vec<&'static str>,
}

fn record_role_name(record: &AgentRecord) -> &'static str {
    match record {
        AgentRecord::Llm(Message::User(_)) => "user",
        AgentRecord::Llm(Message::Assistant(_)) => "assistant",
        AgentRecord::Llm(Message::ToolResult(_)) => "tool_result",
        AgentRecord::Custom { .. } => "custom",
    }
}

fn snapshot_completed_turn(phase: &'static str, turn: &CompletedTurn<'_>) -> CompletedTurnSnapshot {
    CompletedTurnSnapshot {
        phase,
        context_user_texts: turn
            .context
            .records
            .iter()
            .filter_map(text_of_record)
            .map(str::to_owned)
            .collect(),
        new_message_roles: turn.new_messages.iter().map(record_role_name).collect(),
    }
}

struct NewMessagesTurnPolicy {
    snapshots: Arc<Mutex<Vec<CompletedTurnSnapshot>>>,
    first_update: Mutex<Option<(AgentControl, NextTurn)>>,
}

impl TurnPolicy for NewMessagesTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.snapshots).push(snapshot_completed_turn("prepare", &turn));
            let first_update = lock(&self.first_update).take();
            if let Some((control, next)) = first_update {
                control
                    .steer(user("steering", "continue"))
                    .await
                    .map_err(|error| TurnPolicyError::new(error.to_string()))?;
                return Ok(next);
            }
            Ok(NextTurn::default())
        })
    }

    fn should_stop<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.snapshots).push(snapshot_completed_turn("should_stop", &turn));
            Ok(false)
        })
    }
}

#[test]
fn agent_completed_turn_new_messages_match_pi_invocation_boundary() {
    // §10.9 Context phases. Pi basis: packages/agent/src/types.ts:125 and
    // agent-loop.ts:93-120,226-252 pass cumulative per-invocation
    // `newMessages` to both turn callbacks independently of context replacement.
    let prompt_snapshots = Arc::new(Mutex::new(Vec::new()));
    let mut prompt_agent = Agent::new(
        Arc::new(ScriptedRuntime::new([
            text_response("first"),
            text_response("second"),
        ])),
        state(),
        ToolRegistry::new(),
    )
    .unwrap();
    prompt_agent
        .set_turn_policy(Arc::new(NewMessagesTurnPolicy {
            snapshots: prompt_snapshots.clone(),
            first_update: Mutex::new(Some((
                prompt_agent.control(),
                NextTurn {
                    context: Some(AgentRunContext {
                        system_prompt: "replacement prompt".into(),
                        records: vec![user("replacement", "replacement context")],
                        tools: ToolRegistry::new(),
                    }),
                    model: None,
                    reasoning: None,
                },
            ))),
        }))
        .unwrap();

    collect(
        prompt_agent.prompt_records([user("prompt", "prompt message")], CancellationToken::new()),
    );

    assert_eq!(
        lock(&prompt_snapshots).as_slice(),
        [
            CompletedTurnSnapshot {
                phase: "prepare",
                context_user_texts: vec!["prompt message".into()],
                new_message_roles: vec!["user", "assistant"],
            },
            CompletedTurnSnapshot {
                phase: "should_stop",
                context_user_texts: vec!["replacement context".into()],
                new_message_roles: vec!["user", "assistant"],
            },
            CompletedTurnSnapshot {
                phase: "prepare",
                context_user_texts: vec!["replacement context".into(), "continue".into()],
                new_message_roles: vec!["user", "assistant", "user", "assistant"],
            },
            CompletedTurnSnapshot {
                phase: "should_stop",
                context_user_texts: vec!["replacement context".into(), "continue".into()],
                new_message_roles: vec!["user", "assistant", "user", "assistant"],
            },
        ]
    );

    let continuation_snapshots = Arc::new(Mutex::new(Vec::new()));
    let mut continuation_state = state();
    continuation_state
        .transcript
        .push(user("prior", "prior history"));
    let mut continuation_agent = Agent::new(
        Arc::new(ScriptedRuntime::new([text_response("continued")])),
        continuation_state,
        ToolRegistry::new(),
    )
    .unwrap();
    continuation_agent
        .set_turn_policy(Arc::new(NewMessagesTurnPolicy {
            snapshots: continuation_snapshots.clone(),
            first_update: Mutex::new(None),
        }))
        .unwrap();

    collect(
        continuation_agent
            .continue_run(CancellationToken::new())
            .unwrap(),
    );

    assert_eq!(
        lock(&continuation_snapshots).as_slice(),
        [
            CompletedTurnSnapshot {
                phase: "prepare",
                context_user_texts: vec!["prior history".into()],
                new_message_roles: vec!["assistant"],
            },
            CompletedTurnSnapshot {
                phase: "should_stop",
                context_user_texts: vec!["prior history".into()],
                new_message_roles: vec!["assistant"],
            },
        ]
    );
}

#[test]
fn agent_prepare_next_turn_can_replace_model() {
    // §10.9 Context phases. Pi basis: agent-loop.ts applies
    // nextTurnSnapshot.model before the next streamFunction call.
    let replacement = ModelRef::new("alternate", "model-b");
    let requests = run_with_next_turn(NextTurn {
        context: None,
        model: Some(replacement.clone()),
        reasoning: None,
    });

    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, ModelRef::new("scripted", "model-a"));
    assert_eq!(requests[1].model, replacement);
}

#[test]
fn agent_prepare_next_turn_can_replace_reasoning() {
    // §10.9 Context phases. Pi basis: agent-loop.ts applies
    // nextTurnSnapshot.thinkingLevel before the next streamFunction call.
    let requests = run_with_next_turn(NextTurn {
        context: None,
        model: None,
        reasoning: Some(ReasoningLevel::High),
    });

    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].options.reasoning, None);
    assert_eq!(requests[1].options.reasoning, Some(ReasoningLevel::High));
}

struct OrderedTurnPolicy {
    order: Arc<Mutex<Vec<&'static str>>>,
    next: NextTurn,
}

impl TurnPolicy for OrderedTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.order).push("prepare_next_turn");
            Ok(self.next.clone())
        })
    }

    fn should_stop<'a>(
        &'a self,
        turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.order).push("should_stop");
            assert_eq!(turn.context.system_prompt, "second prompt");
            assert_eq!(
                turn.context
                    .records
                    .iter()
                    .filter_map(text_of_record)
                    .collect::<Vec<_>>(),
                ["replacement context"]
            );
            assert!(turn.context.tools.get("first-tool").is_none());
            assert!(turn.context.tools.get("second-tool").is_some());
            Ok(false)
        })
    }
}

#[test]
fn agent_should_stop_runs_after_prepare_next_turn() {
    // §10.9 Context phases. Pi basis: agent-loop.ts awaits prepareNextTurn,
    // applies its snapshot, then invokes shouldStopAfterTurn.
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([text_response("done")])),
        state(),
        tool_registry("first-tool"),
    )
    .unwrap();
    agent
        .set_turn_policy(Arc::new(OrderedTurnPolicy {
            order: order.clone(),
            next: NextTurn {
                context: Some(AgentRunContext {
                    system_prompt: "second prompt".into(),
                    records: vec![user("replacement", "replacement context")],
                    tools: tool_registry("second-tool"),
                }),
                model: None,
                reasoning: None,
            },
        }))
        .unwrap();

    collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));

    assert_eq!(
        lock(&order).as_slice(),
        ["prepare_next_turn", "should_stop"]
    );
}

struct StopBeforeQueuePolicy {
    control: AgentControl,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl TurnPolicy for StopBeforeQueuePolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.order).push("prepare_next_turn");
            self.control
                .steer(user("queued", "must remain queued"))
                .await
                .map_err(|error| TurnPolicyError::new(error.to_string()))?;
            Ok(NextTurn::default())
        })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async move {
            lock(&self.order).push("should_stop");
            Ok(true)
        })
    }
}

#[test]
fn agent_should_stop_precedes_queue_poll() {
    // §10.9 Context phases. Pi basis: agent-loop.ts returns immediately when
    // shouldStopAfterTurn is true, before getSteeringMessages or follow-up.
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([text_response("done")])),
        state(),
        ToolRegistry::new(),
    )
    .unwrap();
    agent
        .set_turn_policy(Arc::new(StopBeforeQueuePolicy {
            control: agent.control(),
            order: order.clone(),
        }))
        .unwrap();

    collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));

    assert_eq!(
        lock(&order).as_slice(),
        ["prepare_next_turn", "should_stop"]
    );
    assert_eq!(agent.clear_steering_queue(), 1);
    assert!(
        agent
            .state()
            .transcript
            .iter()
            .all(|record| record.message_id() != Some(&MessageId::new("queued")))
    );
}

struct OverrideContextPolicy;

impl ContextPolicy for OverrideContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            let options = SimpleGenerationOptions {
                max_output_tokens: Some(321),
                temperature: Some(0.25),
                reasoning: Some(ReasoningLevel::Xhigh),
                ..SimpleGenerationOptions::default()
            };
            Ok(PreparedAgentRecords {
                records: state.records.to_vec(),
                model_override: Some(ModelRef::new("override", "model-c")),
                options_override: Some(options),
                report: Some(HandoffReport::unchanged(ModelFingerprint::new(
                    "override", "test-api", "model-c",
                ))),
            })
        })
    }
}

impl LocalContextPolicy for OverrideContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move {
            Ok(PreparedAgentRecords {
                records: state.records.to_vec(),
                model_override: Some(ModelRef::new("local-override", "model-d")),
                options_override: None,
                report: Some(HandoffReport::unchanged(ModelFingerprint::new(
                    "local-override",
                    "test-api",
                    "model-d",
                ))),
            })
        })
    }
}

#[test]
fn agent_context_policy_can_override_model_and_options() {
    // Architecture v2 part 1 §4.8. Pi basis: prepareNextTurn can switch model
    // and reasoning state; the native context seam additionally permits a
    // complete per-request options replacement without injecting Models.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([text_response("done")]),
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    agent
        .set_context_policy(Arc::new(OverrideContextPolicy))
        .unwrap();

    let events = collect(agent.prompt_records([user("prompt", "start")], CancellationToken::new()));

    let requests = lock(&requests);
    assert_eq!(requests[0].model, ModelRef::new("override", "model-c"));
    assert_eq!(requests[0].options.max_output_tokens, Some(321));
    assert_eq!(requests[0].options.temperature, Some(0.25));
    assert_eq!(requests[0].options.reasoning, Some(ReasoningLevel::Xhigh));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ContextPrepared { target, report, .. }
            if target == &ModelRef::new("override", "model-c")
                && report.target == ModelFingerprint::new("override", "test-api", "model-c")
    )));

    let mut local_agent = LocalAgent::new(
        Rc::new(ScriptedRuntime::new([text_response("local done")])),
        state(),
        LocalToolRegistry::new(),
    )
    .unwrap();
    local_agent
        .set_context_policy(Rc::new(OverrideContextPolicy))
        .unwrap();
    let local_events = collect_local(
        local_agent.prompt_records([user("local-prompt", "start")], CancellationToken::new()),
    );
    assert!(local_events.iter().any(|event| matches!(
        event,
        AgentEvent::ContextPrepared { target, report, .. }
            if target == &ModelRef::new("local-override", "model-d")
                && report.target
                    == ModelFingerprint::new("local-override", "test-api", "model-d")
    )));
}

#[test]
fn agent_default_projector_matches_pi_convert_to_llm() {
    // Pi basis: packages/agent/src/agent.ts defaultConvertToLlm retains the
    // three canonical roles and filters application-defined custom messages.
    let records = vec![
        user("user", "visible"),
        AgentRecord::Custom {
            type_name: "notification".into(),
            payload: to_raw_value(&json!({"text":"UI only"})).unwrap(),
        },
    ];

    let messages = block_on(<DefaultMessageProjector as MessageProjector>::project(
        &DefaultMessageProjector,
        &records,
    ))
    .unwrap();

    assert_eq!(messages.len(), 1);
    assert_eq!(text_of_message(&messages[0]), Some("visible"));
}
