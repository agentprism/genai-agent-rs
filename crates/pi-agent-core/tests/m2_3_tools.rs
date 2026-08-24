use futures_executor::block_on;
use futures_util::{StreamExt, task::noop_waker_ref};
use pi_agent_core::*;
use pi_ai::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue, value::to_raw_value};
use std::{
    future::poll_fn,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Poll, Waker},
};

type ToolHandler = dyn Fn(
        ToolCallContext,
        Arc<dyn ToolUpdateSink>,
        CancellationToken,
    ) -> SendBoxFuture<'static, Result<ToolOutput, ToolError>>
    + Send
    + Sync;

struct TestTool {
    spec: ToolSpec,
    mode: ToolExecutionMode,
    handler: Arc<ToolHandler>,
}

impl TestTool {
    fn new(
        name: &str,
        parameters: Value,
        mode: ToolExecutionMode,
        handler: impl Fn(
            ToolCallContext,
            Arc<dyn ToolUpdateSink>,
            CancellationToken,
        ) -> SendBoxFuture<'static, Result<ToolOutput, ToolError>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: name.into(),
                description: format!("{name} test tool"),
                parameters,
                constrained_sampling: None,
            },
            mode,
            handler: Arc::new(handler),
        }
    }
}

impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.mode
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        (self.handler)(context, updates, cancellation)
    }
}

struct NeverSendTool {
    spec: ToolSpec,
}

impl NeverSendTool {
    fn new(name: &str) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: name.into(),
                description: format!("{name} send test tool"),
                parameters: object_schema(),
                constrained_sampling: None,
            },
        }
    }
}

impl Tool for NeverSendTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        panic!("a truncated send batch must not inspect tool execution mode")
    }

    fn execute(
        &self,
        _context: ToolCallContext,
        _updates: Arc<dyn ToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async { panic!("a truncated send tool call must never execute") })
    }
}

struct NeverLocalTool {
    spec: ToolSpec,
}

impl NeverLocalTool {
    fn new(name: &str) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: name.into(),
                description: format!("{name} local test tool"),
                parameters: object_schema(),
                constrained_sampling: None,
            },
        }
    }
}

impl LocalTool for NeverLocalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        panic!("a truncated local batch must not inspect tool execution mode")
    }

    fn execute(
        &self,
        _context: ToolCallContext,
        _updates: Rc<dyn LocalToolUpdateSink>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async { panic!("a truncated local tool call must never execute") })
    }
}

type AuthorizeHandler =
    dyn for<'a> Fn(BeforeToolCall<'a>) -> Result<ToolAuthorization, AgentError> + Send + Sync;
type FinalizeHandler =
    dyn for<'a> Fn(AfterToolCall<'a>) -> Result<ToolOutputPatch, AgentError> + Send + Sync;

struct TestPolicy {
    authorize: Arc<AuthorizeHandler>,
    finalize: Arc<FinalizeHandler>,
}

impl Default for TestPolicy {
    fn default() -> Self {
        Self {
            authorize: Arc::new(|_| Ok(ToolAuthorization::Allow)),
            finalize: Arc::new(|_| Ok(ToolOutputPatch::default())),
        }
    }
}

impl ToolPolicy for TestPolicy {
    fn authorize<'a>(
        &'a self,
        context: BeforeToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolAuthorization, AgentError>> {
        let result = (self.authorize)(context);
        Box::pin(async move { result })
    }

    fn finalize<'a>(
        &'a self,
        context: AfterToolCall<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<ToolOutputPatch, AgentError>> {
        let result = (self.finalize)(context);
        Box::pin(async move { result })
    }
}

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    open: bool,
    waker: Option<Waker>,
}

impl Gate {
    async fn wait(&self) {
        poll_fn(|context| {
            let mut state = lock(&self.state);
            if state.open {
                Poll::Ready(())
            } else {
                state.waker = Some(context.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    fn open(&self) {
        let waker = {
            let mut state = lock(&self.state);
            state.open = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn object_schema() -> Value {
    json!({"type":"object"})
}

fn value_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"value":{"type":"string"}},
        "required":["value"],
        "additionalProperties":false
    })
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(id),
        name: name.into(),
        arguments,
    }
}

fn assistant(calls: Vec<ToolCall>, finish: AssistantFinishReason) -> AssistantMessage {
    let provider = ProviderId::new("scripted");
    let api = ApiId::new("scripted");
    let model = ModelId::new("test-model");
    AssistantMessage {
        id: MessageId::new("assistant-tools"),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: model.clone(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| ContentBlock::ToolCall {
                id: ContentBlockId::new(format!("tool-block-{index}")),
                call,
            })
            .collect(),
        replay: ReplayEnvelope::new(ReplayScope::new(provider, api, model.clone(), model)),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: finish,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::default(),
    }
}

fn text_output(text: &str) -> ToolOutput {
    ToolOutput::new(vec![ToolResultContent::Text {
        id: ContentBlockId::new(format!("output-{text}")),
        text: text.into(),
    }])
}

fn update(text: &str) -> ToolUpdate {
    ToolUpdate::from(text_output(text))
}

fn text_content(output: &ToolOutput) -> &str {
    match &output.content[0] {
        ToolResultContent::Text { text, .. } => text,
        ToolResultContent::Image { .. } => panic!("expected text output"),
    }
}

fn registry(tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool).unwrap();
    }
    registry
}

fn send_run_context(tools: &ToolRegistry) -> AgentContext {
    AgentRunContext {
        system_prompt: String::new(),
        records: Vec::new(),
        tools: tools.clone(),
    }
}

fn local_run_context(tools: &LocalToolRegistry) -> LocalAgentContext {
    AgentRunContext {
        system_prompt: String::new(),
        records: Vec::new(),
        tools: tools.clone(),
    }
}

fn run_batch(
    scheduler: &ToolScheduler,
    tools: &ToolRegistry,
    assistant: &AssistantMessage,
    mode: ToolExecutionMode,
    cancellation: CancellationToken,
) -> ToolBatchOutcome {
    let calls = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call.clone()),
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. } => None,
        })
        .collect::<Vec<_>>();
    let context = send_run_context(tools);
    block_on(scheduler.execute_batch(
        tools,
        ToolBatchRequest {
            assistant,
            calls: &calls,
            context: &context,
            configured_mode: mode,
            cancellation,
        },
    ))
}

fn success_tool(
    name: &str,
    mode: ToolExecutionMode,
    log: Arc<Mutex<Vec<String>>>,
) -> Arc<dyn Tool> {
    let name_owned = name.to_owned();
    Arc::new(TestTool::new(
        name,
        object_schema(),
        mode,
        move |context, _updates, _cancellation| {
            let log = log.clone();
            let name = name_owned.clone();
            Box::pin(async move {
                lock(&log).push(context.call.id.as_str().to_owned());
                Ok(text_output(&name))
            })
        },
    ))
}

fn state() -> AgentState {
    AgentState::new(
        "tool tests",
        ModelRef::new("scripted", "test-model"),
        ReasoningLevel::Off,
    )
}

fn user() -> AgentRecord {
    AgentRecord::Llm(Message::User(UserMessage {
        id: MessageId::new("user-1"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("user-1-text"),
            text: "run tools".into(),
        }],
        timestamp: Timestamp::default(),
    }))
}

fn scripted_tool_response(calls: &[ToolCall], finish: AssistantFinishReason) -> ScriptedResponse {
    let mut assembler = AssistantAssembler::new();
    let mut events = vec![AssistantEvent::MessageStarted {
        message_id: MessageId::new("assistant-tools"),
        provider: ProviderId::new("scripted"),
        api: ApiId::new("scripted"),
        model: ModelId::new("test-model"),
    }];
    assembler.apply(&events[0]).unwrap();
    for (index, call) in calls.iter().enumerate() {
        let block_id = ContentBlockId::new(format!("tool-block-{index}"));
        for event in [
            AssistantEvent::ContentBlockStarted {
                block_id: block_id.clone(),
                content_index: u32::try_from(index).unwrap(),
                kind: ContentBlockKind::ToolCall,
            },
            AssistantEvent::ToolCallMetadata {
                block_id: block_id.clone(),
                call_id: call.id.clone(),
                name: Some(call.name.clone()),
            },
            AssistantEvent::ToolArgumentsDelta {
                block_id: block_id.clone(),
                delta: serde_json::to_string(&call.arguments).unwrap(),
            },
            AssistantEvent::ContentBlockFinished { block_id },
        ] {
            assembler.apply(&event).unwrap();
            events.push(event);
        }
    }
    let message = assembler
        .finish_completed(AssistantFinish {
            reason: finish,
            raw_provider_reason: None,
            error: None,
        })
        .unwrap();
    events.push(match finish {
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => AssistantEvent::Finished { message },
        AssistantFinishReason::Error | AssistantFinishReason::Aborted => {
            panic!("tool fixture requires a successful provider terminal")
        }
    });
    ScriptedResponse::events(events)
}

fn agent_with_tools(
    tools: ToolRegistry,
    calls: &[ToolCall],
    finish: AssistantFinishReason,
) -> Agent {
    Agent::new(
        Arc::new(ScriptedRuntime::new([
            scripted_tool_response(calls, finish),
            text_response("done"),
        ])),
        state(),
        tools,
    )
    .unwrap()
}

fn collect_agent(agent: &mut Agent, cancellation: CancellationToken) -> Vec<AgentEvent> {
    block_on(
        agent
            .prompt_records([user()], cancellation)
            .collect::<Vec<_>>(),
    )
}

fn tool_event_trace(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionStarted { call } => {
                Some(format!("start:{}", call.id.as_str()))
            }
            AgentEvent::ToolExecutionFinished { call_id, .. } => {
                Some(format!("finish:{}", call_id.as_str()))
            }
            AgentEvent::MessageStarted {
                message_id,
                role: MessageRole::ToolResult,
            } => Some(format!("message_start:{}", message_id.as_str())),
            AgentEvent::MessageCommitted {
                message: AgentRecord::Llm(Message::ToolResult(result)),
            } => Some(format!("message_commit:{}", result.tool_call_id.as_str())),
            _ => None,
        })
        .collect()
}

#[derive(Clone)]
struct RecordingRuntime {
    scripted: ScriptedRuntime,
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
            ModelRuntime::stream(&self.scripted, request, cancellation).await
        })
    }
}

#[test]
fn tool_registration_order_is_preserved_for_model_exposure() {
    // Pi basis: agent.ts retains the caller's AgentTool[] order in state and
    // createContextSnapshot; agent-loop.ts exposes that same array to the model.
    let mut tools = ToolRegistry::new();
    for name in ["zeta", "alpha", "middle"] {
        tools
            .register(Arc::new(TestTool::new(
                name,
                object_schema(),
                ToolExecutionMode::Parallel,
                |_context, _updates, _cancellation| Box::pin(async { Ok(text_output("ok")) }),
            )))
            .unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        scripted: ScriptedRuntime::new([text_response("done")]),
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Arc::new(runtime), state(), tools).unwrap();
    collect_agent(&mut agent, CancellationToken::new());

    let requests = lock(&requests);
    let exposed_names = requests[0]
        .context
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(exposed_names, ["zeta", "alpha", "middle"]);
}

#[test]
fn tool_unknown_name_becomes_error_result() {
    // §10.9 Tools. Pi basis: agent-loop.ts prepareToolCall returns an immediate
    // Tool <name> not found error result.
    let message = assistant(
        vec![call("call-0", "missing", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &ToolRegistry::new(),
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(outcome.source_order.len(), 1);
    assert!(outcome.source_order[0].is_error);
    assert!(text_content(&outcome.source_order[0].output).contains("not found"));
}

#[test]
fn tool_synthesized_failures_include_pi_empty_details() {
    // Pi basis: agent-loop.ts createErrorToolResult always returns details: {};
    // unknown, rejected, truncated, execution, and hook failures share it.
    let message = assistant(
        vec![call("call-0", "missing", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &ToolRegistry::new(),
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(
        outcome.source_order[0]
            .output
            .details
            .as_deref()
            .map(RawValue::get),
        Some("{}")
    );
}

#[derive(Deserialize, JsonSchema)]
struct EchoInput {
    value: String,
}

#[test]
fn tool_prepare_arguments_precedes_validation() {
    // §10.9 Tools. Pi basis: agent-loop.ts prepareToolCallArguments runs before
    // validateToolArguments; pinned Pi also permits post-validation hook mutation.
    let order = Arc::new(Mutex::new(Vec::new()));
    let execute_order = order.clone();
    let typed = TypedTool::<EchoInput, _>::new(
        "echo",
        "typed echo",
        move |_context,
              input: EchoInput,
              _updates,
              _cancellation|
              -> SendBoxFuture<'static, Result<ToolOutput, ToolError>> {
            let order = execute_order.clone();
            Box::pin(async move {
                lock(&order).push(format!("execute:{}", input.value));
                Ok(text_output(&input.value))
            })
        },
    )
    .unwrap();
    let prepare_order = order.clone();
    let mut tools = ToolRegistry::new();
    tools
        .register_with_argument_preparer(
            Arc::new(typed),
            Arc::new(move |arguments: &Value| {
                lock(&prepare_order).push("prepare".into());
                Ok(json!({"value":arguments["legacy"]}))
            }),
        )
        .unwrap();
    let authorize_order = order.clone();
    let policy = TestPolicy {
        authorize: Arc::new(move |context| {
            lock(&authorize_order).push("before".into());
            context.args["value"] = json!("mutated");
            Ok(ToolAuthorization::Allow)
        }),
        ..TestPolicy::default()
    };
    let scheduler = ToolScheduler::new(Arc::new(policy));
    let message = assistant(
        vec![call("call-0", "echo", json!({"legacy":"original"}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(!outcome.source_order[0].is_error);
    assert_eq!(
        lock(&order).as_slice(),
        ["prepare", "before", "execute:mutated"]
    );
}

#[test]
fn tool_validation_precedes_before_hook() {
    // §10.9 Tools. Pi basis: validateToolArguments completes before beforeToolCall.
    let executed = Arc::new(AtomicUsize::new(0));
    let executed_by_tool = executed.clone();
    let tool = TestTool::new(
        "echo",
        value_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let executed = executed_by_tool.clone();
            Box::pin(async move {
                executed.fetch_add(1, Ordering::SeqCst);
                Ok(text_output("unexpected"))
            })
        },
    );
    let before_calls = Arc::new(AtomicUsize::new(0));
    let observed = before_calls.clone();
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        authorize: Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(ToolAuthorization::Allow)
        }),
        ..TestPolicy::default()
    }));
    let tools = registry([Arc::new(tool) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call("call-0", "echo", json!({"value":{"nested":7}}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(outcome.source_order[0].is_error);
    assert_eq!(before_calls.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
}

fn validation_outcome(property_schema: Value, input: Value) -> ToolCallOutcome {
    let parameters = json!({
        "type":"object",
        "properties":{"value":property_schema},
        "required":["value"]
    });
    let tools = registry([Arc::new(TestTool::new(
        "coerce",
        parameters,
        ToolExecutionMode::Parallel,
        |_context, _updates, _cancellation| Box::pin(async { Ok(text_output("ok")) }),
    )) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call("call-0", "coerce", json!({"value":input}))],
        AssistantFinishReason::ToolUse,
    );
    run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    )
    .source_order
    .into_iter()
    .next()
    .unwrap()
}

#[test]
fn tool_validation_coerces_pi_primitive_and_union_inputs() {
    // Pi basis: packages/ai/test/validation.test.ts "coerces serialized plain
    // JSON schemas with AJV-compatible primitive rules" and validation.ts
    // coerceWithJsonSchema. This ports every passing primitive/union case.
    let cases = [
        (json!({"type":"number"}), json!("42"), json!(42)),
        (json!({"type":"number"}), json!(true), json!(1)),
        (json!({"type":"number"}), Value::Null, json!(0)),
        (json!({"type":"integer"}), json!("42"), json!(42)),
        (json!({"type":"boolean"}), json!("true"), json!(true)),
        (json!({"type":"boolean"}), json!("false"), json!(false)),
        (json!({"type":"boolean"}), json!(1), json!(true)),
        (json!({"type":"boolean"}), json!(0), json!(false)),
        (json!({"type":"string"}), Value::Null, json!("")),
        (json!({"type":"string"}), json!(true), json!("true")),
        (json!({"type":"null"}), json!(""), Value::Null),
        (json!({"type":"null"}), json!(0), Value::Null),
        (json!({"type":"null"}), json!(false), Value::Null),
        (json!({"type":["number","string"]}), json!("1"), json!("1")),
        (json!({"type":["boolean","number"]}), json!("1"), json!(1)),
        (
            json!({"anyOf":[{"type":"number"},{"type":"null"}]}),
            json!("42"),
            json!(42),
        ),
    ];

    for (schema, input, expected) in cases {
        let outcome = validation_outcome(schema, input.clone());
        assert!(!outcome.is_error, "input {input} should validate");
        assert_eq!(outcome.effective_arguments, json!({"value":expected}));
        assert_eq!(outcome.call.arguments, json!({"value":input}));
    }
}

#[test]
fn tool_validation_matches_javascript_number_semantics() {
    // Pi basis: packages/ai/src/utils/validation.ts uses JavaScript Number,
    // Number.isFinite, Number.isInteger, and String at this coercion boundary.
    for (input, expected) in [
        ("0x10", json!(16)),
        ("0b10", json!(2)),
        ("0o10", json!(8)),
        ("1.e2", json!(100)),
        ("9007199254740993", json!(9_007_199_254_740_992_u64)),
        ("\u{feff}42", json!(42)),
    ] {
        let outcome = validation_outcome(json!({"type":"number"}), json!(input));
        assert!(!outcome.is_error, "Number({input:?}) should be finite");
        assert_eq!(outcome.effective_arguments["value"], expected);
    }

    let large = validation_outcome(json!({"type":"number"}), json!("0xffffffffffffffff"));
    assert_eq!(
        large.effective_arguments["value"].as_f64(),
        Some(18_446_744_073_709_552_000.0)
    );

    for input in ["+0x10", "-0x10", "+0b10", "\u{0085}42"] {
        assert!(
            validation_outcome(json!({"type":"number"}), json!(input)).is_error,
            "Number({input:?}) is NaN in JavaScript"
        );
    }

    for (input, expected) in [
        (json!(1e21), "1e+21"),
        (json!(1e-7), "1e-7"),
        (json!(1_000_000_000_000_000_100_u64), "1000000000000000100"),
    ] {
        let outcome = validation_outcome(json!({"type":"string"}), input);
        assert_eq!(outcome.effective_arguments["value"], json!(expected));
    }

    for (input, expected) in [(json!(1.0), true), (json!(-0.0), false)] {
        let outcome = validation_outcome(json!({"type":"boolean"}), input);
        assert_eq!(outcome.effective_arguments["value"], json!(expected));
    }
}

#[test]
fn tool_validation_normalizes_optional_nulls_like_pi() {
    // Pi basis: packages/ai/test/validation.test.ts optional-null, referenced
    // nullable, nullable union/oneOf, and nullable-array cases.
    let parameters = json!({
        "type":"object",
        "properties":{
            "path":{"type":"string"},
            "offset":{"type":"number"},
            "nullable":{"anyOf":[{"type":"string"},{"type":"null"}]},
            "metadata":{
                "type":"object",
                "properties":{"enabled":{"type":"boolean"}}
            }
        },
        "required":["path","metadata"]
    });
    let tools = registry([Arc::new(TestTool::new(
        "normalize",
        parameters,
        ToolExecutionMode::Parallel,
        |_context, _updates, _cancellation| Box::pin(async { Ok(text_output("ok")) }),
    )) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call(
            "call-0",
            "normalize",
            json!({
                "path":"file.txt",
                "offset":null,
                "nullable":null,
                "metadata":{"enabled":null}
            }),
        )],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(
        outcome.source_order[0].effective_arguments,
        json!({"path":"file.txt","nullable":null,"metadata":{}})
    );

    for (schema, input) in [
        (
            json!({
                "type":"object",
                "properties":{"value":{"$ref":"#/$defs/value"}},
                "$defs":{"value":{"anyOf":[{"type":"number"},{"type":"null"}]}}
            }),
            json!({"value":null}),
        ),
        (
            json!({
                "type":"object",
                "properties":{"value":{"anyOf":[{"type":"number"},{"type":"null"}]}},
                "required":["value"]
            }),
            json!({"value":null}),
        ),
        (
            json!({
                "type":"object",
                "properties":{"value":{"oneOf":[{"type":"number"},{"type":"null"}]}},
                "required":["value"]
            }),
            json!({"value":null}),
        ),
        (
            json!({
                "type":"object",
                "properties":{"value":{"type":["array","null"],"items":{"type":"string"}}},
                "required":["value"]
            }),
            json!({"value":null}),
        ),
    ] {
        let tools = registry([Arc::new(TestTool::new(
            "nullable",
            schema,
            ToolExecutionMode::Parallel,
            |_context, _updates, _cancellation| Box::pin(async { Ok(text_output("ok")) }),
        )) as Arc<dyn Tool>]);
        let message = assistant(
            vec![call("call-0", "nullable", input)],
            AssistantFinishReason::ToolUse,
        );
        let outcome = run_batch(
            &ToolScheduler::default(),
            &tools,
            &message,
            ToolExecutionMode::Parallel,
            CancellationToken::new(),
        );
        assert!(!outcome.source_order[0].is_error);
        assert_eq!(
            outcome.source_order[0].effective_arguments,
            json!({"value":null})
        );
    }
}

#[derive(Deserialize, JsonSchema)]
struct CoercedNumberInput {
    value: u64,
}

#[test]
fn typed_tool_uses_pi_normalized_arguments_before_policy_and_serde() {
    // Pi basis: packages/ai/src/utils/validation.ts:317–339 returns the cloned,
    // converted arguments; agent-loop.ts passes those to beforeToolCall and
    // execution. Rust then performs the architecture §4.5 serde typed call.
    let typed_values = Arc::new(Mutex::new(Vec::new()));
    let observed_by_tool = typed_values.clone();
    let typed = TypedTool::<CoercedNumberInput, _>::new(
        "typed-number",
        "typed normalized number",
        move |_context,
              input: CoercedNumberInput,
              _updates,
              _cancellation|
              -> SendBoxFuture<'static, Result<ToolOutput, ToolError>> {
            let observed = observed_by_tool.clone();
            Box::pin(async move {
                lock(&observed).push(input.value);
                Ok(text_output("typed"))
            })
        },
    )
    .unwrap();
    let policy_values = Arc::new(Mutex::new(Vec::new()));
    let observed_by_policy = policy_values.clone();
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        authorize: Arc::new(move |context| {
            lock(&observed_by_policy).push(context.args["value"].clone());
            Ok(ToolAuthorization::Allow)
        }),
        ..TestPolicy::default()
    }));
    let tools = registry([Arc::new(typed) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call("call-0", "typed-number", json!({"value":"42"}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(!outcome.source_order[0].is_error);
    assert_eq!(lock(&policy_values).as_slice(), [json!(42)]);
    assert_eq!(lock(&typed_values).as_slice(), [42]);
}

#[test]
fn tool_validation_rejects_invalid_pi_coercions() {
    // Pi basis: packages/ai/test/validation.test.ts invalid serialized-schema
    // coercions remain validation failures.
    for (schema, input) in [
        (json!({"type":"boolean"}), json!("1")),
        (json!({"type":"boolean"}), json!("0")),
        (json!({"type":"null"}), json!("null")),
        (json!({"type":"integer"}), json!("42.1")),
    ] {
        assert!(validation_outcome(schema, input).is_error);
    }
}

#[test]
fn tool_before_hook_can_block() {
    // §10.9 Tools. Pi basis: beforeToolCall with block=true produces an
    // immediate error result and never invokes execute.
    let executed = Arc::new(AtomicBool::new(false));
    let tool_executed = executed.clone();
    let tools = registry([Arc::new(TestTool::new(
        "echo",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let executed = tool_executed.clone();
            Box::pin(async move {
                executed.store(true, Ordering::SeqCst);
                Ok(text_output("unexpected"))
            })
        },
    )) as Arc<dyn Tool>]);
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        authorize: Arc::new(|_| {
            Ok(ToolAuthorization::Block {
                reason: Some("blocked by policy".into()),
                terminate: false,
            })
        }),
        ..TestPolicy::default()
    }));
    let message = assistant(
        vec![call("call-0", "echo", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(outcome.source_order[0].is_error);
    assert_eq!(
        text_content(&outcome.source_order[0].output),
        "blocked by policy"
    );
    assert!(!executed.load(Ordering::SeqCst));
}

#[test]
fn tool_before_hook_can_terminate() {
    // §10.9 Tools. Pi basis: a blocked beforeToolCall result may set terminate.
    let tools = registry([success_tool(
        "echo",
        ToolExecutionMode::Parallel,
        Arc::new(Mutex::new(Vec::new())),
    )]);
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        authorize: Arc::new(|_| {
            Ok(ToolAuthorization::Block {
                reason: None,
                terminate: true,
            })
        }),
        ..TestPolicy::default()
    }));
    let message = assistant(
        vec![call("call-0", "echo", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(outcome.source_order[0].output.terminate);
    assert!(outcome.terminate);
}

#[test]
fn tool_execution_error_becomes_error_result() {
    // §10.9 Tools. Pi basis: executePreparedToolCall catches tool rejection and
    // creates a model-visible error result.
    let tools = registry([Arc::new(TestTool::new(
        "fail",
        object_schema(),
        ToolExecutionMode::Parallel,
        |_context, _updates, _cancellation| {
            Box::pin(async { Err(ToolError::new("failed", "tool exploded")) })
        },
    )) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call("call-0", "fail", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert!(outcome.source_order[0].is_error);
    assert_eq!(
        text_content(&outcome.source_order[0].output),
        "tool exploded"
    );
}

#[test]
fn tool_updates_precede_tool_finished() {
    // §10.9 Tools. Pi basis: executePreparedToolCall awaits accepted update
    // emissions before tool_execution_end. Architecture part 1 §4.3 also
    // requires channel adapters to remain bounded under a synchronously chatty
    // producer.
    let saturated = Arc::new(AtomicBool::new(false));
    let observed_saturation = saturated.clone();
    let tool = Arc::new(TestTool::new(
        "updates",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, updates, _cancellation| {
            let saturated = observed_saturation.clone();
            Box::pin(async move {
                for index in 0..1_024 {
                    if updates.update(update(&format!("partial-{index}"))).is_err() {
                        saturated.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Ok(text_output("final"))
            })
        },
    )) as Arc<dyn Tool>;
    let calls = [call("call-0", "updates", json!({}))];
    let mut agent = agent_with_tools(registry([tool]), &calls, AssistantFinishReason::ToolUse);
    let events = collect_agent(&mut agent, CancellationToken::new());
    let update_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionUpdated { .. }))
        .unwrap();
    let finish_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolExecutionFinished { .. }))
        .unwrap();
    assert!(update_index < finish_index);
    assert!(saturated.load(Ordering::SeqCst));
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdated { .. }))
            .count()
            < 1_024
    );
}

#[test]
fn tool_late_updates_are_ignored() {
    // §10.9 Tools. Pi basis: acceptingUpdates becomes false immediately after
    // the tool promise settles.
    let retained = Arc::new(Mutex::new(None::<Arc<dyn ToolUpdateSink>>));
    let retained_by_tool = retained.clone();
    let tools = registry([Arc::new(TestTool::new(
        "updates",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, updates, _cancellation| {
            let retained = retained_by_tool.clone();
            Box::pin(async move {
                updates.update(update("accepted")).unwrap();
                *lock(&retained) = Some(updates);
                Ok(text_output("final"))
            })
        },
    )) as Arc<dyn Tool>]);
    let message = assistant(
        vec![call("call-0", "updates", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    let calls = [call("call-0", "updates", json!({}))];
    let (outcome, accepted_updates) = block_on(async {
        let scheduler = ToolScheduler::default();
        let context = send_run_context(&tools);
        let mut events = scheduler.execute_batch_events(
            &tools,
            ToolBatchRequest {
                assistant: &message,
                calls: &calls,
                context: &context,
                configured_mode: ToolExecutionMode::Parallel,
                cancellation: CancellationToken::new(),
            },
        );
        let mut accepted_updates = 0;
        let mut outcome = None;
        while let Some(event) = events.next().await {
            match event {
                ToolBatchStreamEvent::CallUpdated { .. } => accepted_updates += 1,
                ToolBatchStreamEvent::BatchFinished { outcome: batch } => {
                    outcome = Some(*batch);
                }
                ToolBatchStreamEvent::BatchStarted { .. }
                | ToolBatchStreamEvent::CallStarted { .. }
                | ToolBatchStreamEvent::CallFinished { .. } => {}
            }
        }
        (outcome.unwrap(), accepted_updates)
    });
    assert_eq!(outcome.source_order.len(), 1);
    assert_eq!(accepted_updates, 1);
    lock(&retained)
        .as_ref()
        .unwrap()
        .update(update("late"))
        .unwrap();
    assert_eq!(accepted_updates, 1);
}

#[test]
fn tool_after_hook_precedes_tool_finished() {
    // §10.9 Tools. Pi basis: finalizeExecutedToolCall runs after execution and
    // before tool_execution_end.
    let order = Arc::new(Mutex::new(Vec::new()));
    let execute_order = order.clone();
    let tool = Arc::new(TestTool::new(
        "echo",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let order = execute_order.clone();
            Box::pin(async move {
                lock(&order).push("execute");
                Ok(text_output("original"))
            })
        },
    )) as Arc<dyn Tool>;
    let finalize_order = order.clone();
    let policy = TestPolicy {
        finalize: Arc::new(move |_| {
            lock(&finalize_order).push("after");
            Ok(ToolOutputPatch::default())
        }),
        ..TestPolicy::default()
    };
    let calls = [call("call-0", "echo", json!({}))];
    let mut agent = agent_with_tools(registry([tool]), &calls, AssistantFinishReason::ToolUse);
    agent.set_tool_policy(Arc::new(policy)).unwrap();
    block_on(async {
        let mut events = agent.prompt_records([user()], CancellationToken::new());
        while let Some(event) = events.next().await {
            if matches!(event, AgentEvent::ToolExecutionFinished { .. }) {
                lock(&order).push("finished");
            }
        }
    });
    assert_eq!(lock(&order).as_slice(), ["execute", "after", "finished"]);
}

fn patched_outcome(patch: ToolOutputPatch, failing: bool) -> ToolCallOutcome {
    let tool = Arc::new(TestTool::new(
        "patch",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            Box::pin(async move {
                if failing {
                    Err(ToolError::new("failed", "original failure"))
                } else {
                    Ok(text_output("original"))
                }
            })
        },
    )) as Arc<dyn Tool>;
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        finalize: Arc::new(move |_| Ok(patch.clone())),
        ..TestPolicy::default()
    }));
    let tools = registry([tool]);
    let message = assistant(
        vec![call("call-0", "patch", json!({}))],
        AssistantFinishReason::ToolUse,
    );
    run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    )
    .source_order
    .into_iter()
    .next()
    .unwrap()
}

#[test]
fn tool_after_hook_can_replace_content() {
    // §10.9 Tools. Pi basis: afterToolCall content replaces the complete array.
    let outcome = patched_outcome(
        ToolOutputPatch {
            content: Some(text_output("replacement").content),
            ..ToolOutputPatch::default()
        },
        false,
    );
    assert_eq!(text_content(&outcome.output), "replacement");
}

#[test]
fn tool_after_hook_can_replace_details() {
    // §10.9 Tools. Pi basis: afterToolCall details replaces the complete value.
    let details = to_raw_value(&json!({"replacement":true})).unwrap();
    let outcome = patched_outcome(
        ToolOutputPatch {
            details: Some(details),
            ..ToolOutputPatch::default()
        },
        false,
    );
    assert_eq!(
        outcome.output.details.as_deref().unwrap().get(),
        "{\"replacement\":true}"
    );
}

#[test]
fn tool_after_hook_can_replace_usage() {
    // §10.9 Tools. Pi basis: afterToolCall usage replaces tool-attributed usage.
    let replacement = Usage {
        input_tokens: 1,
        output_tokens: 2,
        reasoning_tokens: Some(3),
        cache_read_tokens: Some(4),
        cache_write_tokens: Some(5),
        cache_write_one_hour_tokens: None,
        source: UsageSource::Estimated,
    };
    let outcome = patched_outcome(
        ToolOutputPatch {
            usage: Some(replacement.clone()),
            ..ToolOutputPatch::default()
        },
        false,
    );
    assert_eq!(outcome.output.usage, Some(replacement));
}

#[test]
fn tool_after_hook_can_change_error_state() {
    // §10.9 Tools. Pi basis: afterToolCall isError replaces execution error state.
    let outcome = patched_outcome(
        ToolOutputPatch {
            is_error: Some(false),
            ..ToolOutputPatch::default()
        },
        true,
    );
    assert!(!outcome.is_error);
}

#[test]
fn tool_after_hook_can_terminate() {
    // §10.9 Tools. Pi basis: afterToolCall terminate replaces the tool hint.
    let outcome = patched_outcome(
        ToolOutputPatch {
            terminate: Some(true),
            ..ToolOutputPatch::default()
        },
        false,
    );
    assert!(outcome.output.terminate);
}

#[test]
fn tool_any_sequential_tool_forces_sequential_batch() {
    // §10.9 Tools. Pi basis: agent-loop.ts chooses sequential when any resolved
    // call targets a tool whose executionMode is sequential. Its sequential
    // loop emits and commits each result before starting the next call.
    let log = Arc::new(Mutex::new(Vec::new()));
    let tools = registry([
        success_tool("first", ToolExecutionMode::Parallel, log.clone()),
        success_tool("second", ToolExecutionMode::Sequential, log.clone()),
    ]);
    let calls = [
        call("call-0", "first", json!({})),
        call("call-1", "second", json!({})),
    ];
    let message = assistant(calls.to_vec(), AssistantFinishReason::ToolUse);
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(outcome.plan, ToolExecutionPlan::SequentialBatch);
    assert_eq!(lock(&log).as_slice(), ["call-0", "call-1"]);

    let mut agent = agent_with_tools(tools, &calls, AssistantFinishReason::ToolUse);
    let events = collect_agent(&mut agent, CancellationToken::new());
    assert_eq!(
        tool_event_trace(&events),
        [
            "start:call-0",
            "finish:call-0",
            "message_start:assistant-tools-tool-result-0",
            "message_commit:call-0",
            "start:call-1",
            "finish:call-1",
            "message_start:assistant-tools-tool-result-1",
            "message_commit:call-1",
        ]
    );
}

#[test]
fn tool_parallel_preflight_is_source_order() {
    // §10.9 Tools. Pi basis: parallel agent-loop preflights calls in source order
    // before invoking allowed executions.
    let preflight = Arc::new(Mutex::new(Vec::new()));
    let observed = preflight.clone();
    let scheduler = ToolScheduler::new(Arc::new(TestPolicy {
        authorize: Arc::new(move |context| {
            lock(&observed).push(context.tool_call.id.as_str().to_owned());
            Ok(ToolAuthorization::Allow)
        }),
        ..TestPolicy::default()
    }));
    let log = Arc::new(Mutex::new(Vec::new()));
    let tools = registry([success_tool("echo", ToolExecutionMode::Parallel, log)]);
    let message = assistant(
        vec![
            call("call-0", "echo", json!({})),
            call("call-1", "echo", json!({})),
            call("call-2", "echo", json!({})),
        ],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &scheduler,
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(lock(&preflight).as_slice(), ["call-0", "call-1", "call-2"]);
    assert_eq!(
        outcome
            .source_order
            .iter()
            .map(|result| result.preflight_index)
            .collect::<Vec<_>>(),
        [PreflightIndex(0), PreflightIndex(1), PreflightIndex(2)]
    );

    // An immediate preflight failure completes before Pi starts the next call
    // lifecycle (agent-loop.ts:499–533), even in parallel mode.
    let lifecycle_calls = [
        call("call-immediate", "missing", json!({})),
        call("call-next", "echo", json!({})),
    ];
    let mut agent = agent_with_tools(
        tools.clone(),
        &lifecycle_calls,
        AssistantFinishReason::ToolUse,
    );
    let lifecycle = collect_agent(&mut agent, CancellationToken::new())
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionStarted { call } => {
                Some(format!("start:{}", call.id.as_str()))
            }
            AgentEvent::ToolExecutionFinished { call_id, .. } => {
                Some(format!("finish:{}", call_id.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        &lifecycle[..3],
        [
            "start:call-immediate",
            "finish:call-immediate",
            "start:call-next"
        ]
    );
}

fn completion_order_fixture() -> (ToolRegistry, AssistantMessage) {
    let gate = Arc::new(Gate::default());
    let slow_gate = gate.clone();
    let slow = Arc::new(TestTool::new(
        "slow",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let gate = slow_gate.clone();
            Box::pin(async move {
                gate.wait().await;
                Ok(text_output("slow"))
            })
        },
    )) as Arc<dyn Tool>;
    let fast_gate = gate;
    let fast = Arc::new(TestTool::new(
        "fast",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let gate = fast_gate.clone();
            Box::pin(async move {
                gate.open();
                Ok(text_output("fast"))
            })
        },
    )) as Arc<dyn Tool>;
    (
        registry([slow, fast]),
        assistant(
            vec![
                call("call-0", "slow", json!({})),
                call("call-1", "fast", json!({})),
            ],
            AssistantFinishReason::ToolUse,
        ),
    )
}

#[test]
fn tool_parallel_completion_events_are_completion_order() {
    // §10.9 Tools. Pi basis: agent-loop.ts:499–533 and :675–699 exposes
    // updates and tool_execution_end in actual cross-call order while slower
    // parallel calls remain active. Result messages are tested separately.
    let slow_gate = Arc::new(Gate::default());
    let gate_for_tool = slow_gate.clone();
    let slow = Arc::new(TestTool::new(
        "slow",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let gate = gate_for_tool.clone();
            Box::pin(async move {
                gate.wait().await;
                Ok(text_output("slow"))
            })
        },
    )) as Arc<dyn Tool>;
    let fast = Arc::new(TestTool::new(
        "fast",
        object_schema(),
        ToolExecutionMode::Parallel,
        |_context, updates, _cancellation| {
            Box::pin(async move {
                updates.update(update("fast-update")).unwrap();
                Ok(text_output("fast"))
            })
        },
    )) as Arc<dyn Tool>;
    let calls = [
        call("call-0", "slow", json!({})),
        call("call-1", "fast", json!({})),
    ];
    let mut agent = agent_with_tools(
        registry([slow, fast]),
        &calls,
        AssistantFinishReason::ToolUse,
    );
    let mut stream = agent.prompt_records([user()], CancellationToken::new());
    let mut events = Vec::new();
    let mut task_context = std::task::Context::from_waker(noop_waker_ref());
    for _ in 0..128 {
        match stream.as_mut().poll_next(&mut task_context) {
            Poll::Ready(Some(event)) => {
                let fast_finished = matches!(
                    &event,
                    AgentEvent::ToolExecutionFinished { call_id, .. }
                        if call_id.as_str() == "call-1"
                );
                events.push(event);
                if fast_finished {
                    break;
                }
            }
            Poll::Ready(None) => panic!("run finished while the slow tool was still gated"),
            Poll::Pending => {}
        }
    }

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionUpdated { call_id, .. }
            if call_id.as_str() == "call-1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished { call_id, .. }
            if call_id.as_str() == "call-1"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished { call_id, .. }
            if call_id.as_str() == "call-0"
    )));

    slow_gate.open();
    events.extend(block_on(stream.collect::<Vec<_>>()));
    let completion_ids = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionFinished { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completion_ids, ["call-1", "call-0"]);
}

#[test]
fn tool_parallel_result_messages_are_source_order() {
    // §10.9 Tools. Pi basis: Promise.all preserves assistant source slots for
    // tool-result message artifacts after every completion event has settled.
    let (tools, message) = completion_order_fixture();
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_eq!(
        outcome
            .source_order
            .iter()
            .map(|result| result.call.id.as_str())
            .collect::<Vec<_>>(),
        ["call-0", "call-1"]
    );
    assert_eq!(
        outcome
            .source_order
            .iter()
            .map(|result| result.source_index)
            .collect::<Vec<_>>(),
        [SourceIndex(0), SourceIndex(1)]
    );

    let (agent_tools, agent_message) = completion_order_fixture();
    let calls = agent_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut agent = agent_with_tools(agent_tools, &calls, AssistantFinishReason::ToolUse);
    let events = collect_agent(&mut agent, CancellationToken::new());
    assert_eq!(
        tool_event_trace(&events),
        [
            "start:call-0",
            "start:call-1",
            "finish:call-1",
            "finish:call-0",
            "message_start:assistant-tools-tool-result-0",
            "message_commit:call-0",
            "message_start:assistant-tools-tool-result-1",
            "message_commit:call-1",
        ]
    );
}

#[test]
fn tool_parallel_turn_results_are_source_order() {
    // §10.9 Tools. Pi basis: turn_end toolResults preserves assistant source order.
    let (tools, message) = completion_order_fixture();
    let calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut agent = agent_with_tools(tools, &calls, AssistantFinishReason::ToolUse);
    let events = collect_agent(&mut agent, CancellationToken::new());
    let ids = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnFinished { outcome } if outcome.tool_result_message_ids.len() == 2 => {
                Some(
                    outcome
                        .tool_result_message_ids
                        .iter()
                        .map(MessageId::as_str)
                        .collect::<Vec<_>>(),
                )
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        ids,
        [
            "assistant-tools-tool-result-0",
            "assistant-tools-tool-result-1"
        ]
    );
}

#[test]
fn tool_batch_terminates_only_when_all_results_terminate() {
    // §10.9 Tools. Pi basis: shouldTerminateToolBatch requires a nonempty batch
    // and every finalized result's terminate flag.
    let tools = registry([Arc::new(TestTool::new(
        "terminate",
        object_schema(),
        ToolExecutionMode::Parallel,
        |context, _updates, _cancellation| {
            Box::pin(async move {
                let mut output = text_output(context.call.id.as_str());
                output.terminate = context.call.arguments["terminate"] == json!(true);
                Ok(output)
            })
        },
    )) as Arc<dyn Tool>]);
    let mixed = assistant(
        vec![
            call("call-0", "terminate", json!({"terminate":true})),
            call("call-1", "terminate", json!({"terminate":false})),
        ],
        AssistantFinishReason::ToolUse,
    );
    assert!(
        !run_batch(
            &ToolScheduler::default(),
            &tools,
            &mixed,
            ToolExecutionMode::Parallel,
            CancellationToken::new(),
        )
        .terminate
    );
    let all = assistant(
        vec![
            call("call-0", "terminate", json!({"terminate":true})),
            call("call-1", "terminate", json!({"terminate":true})),
        ],
        AssistantFinishReason::ToolUse,
    );
    assert!(
        run_batch(
            &ToolScheduler::default(),
            &tools,
            &all,
            ToolExecutionMode::Parallel,
            CancellationToken::new(),
        )
        .terminate
    );
}

#[test]
fn tool_length_truncated_calls_are_never_executed() {
    // §10.9 Tools. Pi basis: failToolCallsFromTruncatedMessage rejects every
    // call when stopReason is length. The assistant terminal reason is the
    // authority; the dedicated path never resolves execution modes or tools.
    let executed = Arc::new(AtomicUsize::new(0));
    let observed = executed.clone();
    let tool = Arc::new(TestTool::new(
        "echo",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let executed = observed.clone();
            Box::pin(async move {
                executed.fetch_add(1, Ordering::SeqCst);
                Ok(text_output("unexpected"))
            })
        },
    )) as Arc<dyn Tool>;
    let calls = [call("call-0", "echo", json!({"partial":"value"}))];
    let mut agent = agent_with_tools(registry([tool]), &calls, AssistantFinishReason::Length);
    let events = collect_agent(&mut agent, CancellationToken::new());
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished { is_error: true, .. }
    )));

    let adversarial_message = assistant(calls.to_vec(), AssistantFinishReason::Length);
    let send_tools = registry([Arc::new(NeverSendTool::new("echo")) as Arc<dyn Tool>]);
    let send_outcome = run_batch(
        &ToolScheduler::default(),
        &send_tools,
        &adversarial_message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_truncated_results(&send_outcome, 1);

    let mut local_tools = LocalToolRegistry::new();
    local_tools
        .register(Rc::new(NeverLocalTool::new("echo")))
        .unwrap();
    let local_context = local_run_context(&local_tools);
    let local_outcome = block_on(LocalToolScheduler::default().execute_batch(
        &local_tools,
        ToolBatchRequest {
            assistant: &adversarial_message,
            calls: &calls,
            context: &local_context,
            configured_mode: ToolExecutionMode::Parallel,
            cancellation: CancellationToken::new(),
        },
    ));
    assert_truncated_results(&local_outcome, 1);
}

#[test]
fn tool_length_truncated_calls_each_receive_error_result() {
    // §10.9 Tools. Pi basis: failToolCallsFromTruncatedMessage emits one error
    // tool result for every call in assistant source order, commits it before
    // the next call starts, and does not consult cancellation.
    let log = Arc::new(Mutex::new(Vec::new()));
    let tools = registry([success_tool("echo", ToolExecutionMode::Parallel, log)]);
    let calls = vec![
        call("call-0", "echo", json!({})),
        call("call-1", "echo", json!({})),
    ];
    let message = assistant(calls.clone(), AssistantFinishReason::Length);

    let ordinary = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        CancellationToken::new(),
    );
    assert_truncated_results(&ordinary, 2);

    let pre_cancelled = CancellationToken::new();
    pre_cancelled.cancel();
    let pre_cancelled_send = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Sequential,
        pre_cancelled,
    );
    assert_truncated_results(&pre_cancelled_send, 2);

    let mid_batch = CancellationToken::new();
    let mid_batch_send = block_on(async {
        let scheduler = ToolScheduler::default();
        let context = send_run_context(&tools);
        let mut events = scheduler.execute_batch_events(
            &tools,
            ToolBatchRequest {
                assistant: &message,
                calls: &calls,
                context: &context,
                configured_mode: ToolExecutionMode::Parallel,
                cancellation: mid_batch.clone(),
            },
        );
        let mut first_finished = false;
        while let Some(event) = events.next().await {
            match event {
                ToolBatchStreamEvent::CallFinished { .. } if !first_finished => {
                    first_finished = true;
                    mid_batch.cancel();
                }
                ToolBatchStreamEvent::BatchFinished { outcome } => return *outcome,
                ToolBatchStreamEvent::BatchStarted { .. }
                | ToolBatchStreamEvent::CallStarted { .. }
                | ToolBatchStreamEvent::CallUpdated { .. }
                | ToolBatchStreamEvent::CallFinished { .. } => {}
            }
        }
        panic!("send scheduler ended without a batch outcome")
    });
    assert_truncated_results(&mid_batch_send, 2);

    let mut local_tools = LocalToolRegistry::new();
    local_tools
        .register(Rc::new(NeverLocalTool::new("echo")))
        .unwrap();
    let local_scheduler = LocalToolScheduler::default();
    let pre_cancelled = CancellationToken::new();
    pre_cancelled.cancel();
    let local_context = local_run_context(&local_tools);
    let pre_cancelled_local = block_on(local_scheduler.execute_batch(
        &local_tools,
        ToolBatchRequest {
            assistant: &message,
            calls: &calls,
            context: &local_context,
            configured_mode: ToolExecutionMode::Sequential,
            cancellation: pre_cancelled,
        },
    ));
    assert_truncated_results(&pre_cancelled_local, 2);

    let mid_batch = CancellationToken::new();
    let mid_batch_local = block_on(async {
        let context = local_run_context(&local_tools);
        let mut events = local_scheduler.execute_batch_events(
            &local_tools,
            ToolBatchRequest {
                assistant: &message,
                calls: &calls,
                context: &context,
                configured_mode: ToolExecutionMode::Parallel,
                cancellation: mid_batch.clone(),
            },
        );
        let mut first_finished = false;
        while let Some(event) = events.next().await {
            match event {
                ToolBatchStreamEvent::CallFinished { .. } if !first_finished => {
                    first_finished = true;
                    mid_batch.cancel();
                }
                ToolBatchStreamEvent::BatchFinished { outcome } => return *outcome,
                ToolBatchStreamEvent::BatchStarted { .. }
                | ToolBatchStreamEvent::CallStarted { .. }
                | ToolBatchStreamEvent::CallUpdated { .. }
                | ToolBatchStreamEvent::CallFinished { .. } => {}
            }
        }
        panic!("local scheduler ended without a batch outcome")
    });
    assert_truncated_results(&mid_batch_local, 2);

    let mut agent = agent_with_tools(tools, &calls, AssistantFinishReason::Length);
    let events = collect_agent(&mut agent, CancellationToken::new());
    assert_eq!(
        tool_event_trace(&events),
        [
            "start:call-0",
            "finish:call-0",
            "message_start:assistant-tools-tool-result-0",
            "message_commit:call-0",
            "start:call-1",
            "finish:call-1",
            "message_start:assistant-tools-tool-result-1",
            "message_commit:call-1",
        ]
    );
}

fn assert_truncated_results(outcome: &ToolBatchOutcome, expected: usize) {
    assert_eq!(outcome.source_order.len(), expected);
    assert!(outcome.source_order.iter().all(|result| result.is_error));
    assert!(outcome.source_order.iter().all(|result| {
        let text = text_content(&result.output);
        text.contains("Tool call \"echo\"")
            && text.contains("output token limit")
            && text.contains("Re-issue the tool call with complete arguments.")
    }));
}

#[test]
fn tool_cancellation_stops_new_sequential_calls() {
    // §10.9 Tools. Pi basis: sequential execution checks the abort signal after
    // each finalized call and does not start later source calls.
    let cancellation = CancellationToken::new();
    let cancel_from_tool = cancellation.clone();
    let executed = Arc::new(Mutex::new(Vec::new()));
    let first_log = executed.clone();
    let first = Arc::new(TestTool::new(
        "first",
        object_schema(),
        ToolExecutionMode::Sequential,
        move |context, _updates, _cancellation| {
            let cancellation = cancel_from_tool.clone();
            let log = first_log.clone();
            Box::pin(async move {
                lock(&log).push(context.call.id.as_str().to_owned());
                cancellation.cancel();
                Ok(text_output("first"))
            })
        },
    )) as Arc<dyn Tool>;
    let second_log = executed.clone();
    let second = success_tool("second", ToolExecutionMode::Parallel, second_log);
    let tools = registry([first, second]);
    let message = assistant(
        vec![
            call("call-0", "first", json!({})),
            call("call-1", "second", json!({})),
        ],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        cancellation,
    );
    assert_eq!(outcome.plan, ToolExecutionPlan::SequentialBatch);
    assert_eq!(lock(&executed).as_slice(), ["call-0"]);
    assert_eq!(outcome.source_order.len(), 1);
}

#[test]
fn tool_cancellation_joins_running_parallel_calls() {
    // §10.9 Tools. Pi basis: executeToolCallsParallel awaits Promise.all;
    // architecture §4.6 and §9.1 require the child token and joined futures.
    let cancellation = CancellationToken::new();
    let settled = Arc::new(AtomicBool::new(false));
    let waiter_settled = settled.clone();
    let waiter = Arc::new(TestTool::new(
        "waiter",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, cancellation| {
            let settled = waiter_settled.clone();
            Box::pin(async move {
                cancellation.cancelled().await;
                settled.store(true, Ordering::SeqCst);
                Ok(text_output("joined"))
            })
        },
    )) as Arc<dyn Tool>;
    let cancel_parent = cancellation.clone();
    let canceller = Arc::new(TestTool::new(
        "canceller",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let cancellation = cancel_parent.clone();
            Box::pin(async move {
                cancellation.cancel();
                Ok(text_output("cancelled"))
            })
        },
    )) as Arc<dyn Tool>;
    let tools = registry([waiter, canceller]);
    let message = assistant(
        vec![
            call("call-0", "waiter", json!({})),
            call("call-1", "canceller", json!({})),
        ],
        AssistantFinishReason::ToolUse,
    );
    let outcome = run_batch(
        &ToolScheduler::default(),
        &tools,
        &message,
        ToolExecutionMode::Parallel,
        cancellation,
    );
    assert_eq!(outcome.source_order.len(), 2);
    assert!(settled.load(Ordering::SeqCst));
}

#[test]
fn tool_no_process_or_file_mutation_after_run_finished() {
    // §10.9 Tools. Pi basis: agentLoop awaits executeToolCallsParallel before
    // agent_end; architecture §4.6 forbids post-RunFinished side effects.
    let cancellation = CancellationToken::new();
    let mutations = Arc::new(AtomicUsize::new(0));
    let waiter_mutations = mutations.clone();
    let waiter = Arc::new(TestTool::new(
        "mutator",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, cancellation| {
            let mutations = waiter_mutations.clone();
            Box::pin(async move {
                cancellation.cancelled().await;
                mutations.fetch_add(1, Ordering::SeqCst);
                Ok(text_output("mutation settled"))
            })
        },
    )) as Arc<dyn Tool>;
    let cancel_parent = cancellation.clone();
    let canceller = Arc::new(TestTool::new(
        "canceller",
        object_schema(),
        ToolExecutionMode::Parallel,
        move |_context, _updates, _cancellation| {
            let cancellation = cancel_parent.clone();
            Box::pin(async move {
                cancellation.cancel();
                Ok(text_output("cancelled"))
            })
        },
    )) as Arc<dyn Tool>;
    let calls = [
        call("call-0", "mutator", json!({})),
        call("call-1", "canceller", json!({})),
    ];
    let mut agent = agent_with_tools(
        registry([waiter, canceller]),
        &calls,
        AssistantFinishReason::ToolUse,
    );
    let observed_at_finish = block_on(async {
        let mut events = agent.prompt_records([user()], cancellation);
        let mut count = None;
        while let Some(event) = events.next().await {
            if matches!(event, AgentEvent::RunFinished { .. }) {
                count = Some(mutations.load(Ordering::SeqCst));
            }
        }
        count.unwrap()
    });
    assert_eq!(observed_at_finish, 1);
    assert_eq!(mutations.load(Ordering::SeqCst), observed_at_finish);
}
