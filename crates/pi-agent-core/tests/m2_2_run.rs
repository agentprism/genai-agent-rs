use futures_executor::block_on;
use futures_util::{StreamExt, stream};
use pi_agent_core::*;
use pi_ai::*;
use serde_json::json;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

fn state() -> AgentState {
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
            text: text.into(),
        }],
        timestamp: Timestamp::default(),
    }))
}

fn error() -> PublicError {
    PublicError {
        code: "provider_error".into(),
        message: "provider failed".into(),
        retryable: true,
        provider_code: Some("overloaded".into()),
        status: Some(503),
        request_id: Some("request-1".into()),
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: Some(1),
        cache_read_tokens: Some(2),
        cache_write_tokens: Some(3),
        cache_write_one_hour_tokens: None,
        total_tokens: None,
        source: UsageSource::ProviderReported,
    }
}

fn agent(responses: impl IntoIterator<Item = ScriptedResponse>) -> Agent {
    Agent::new(
        Arc::new(ScriptedRuntime::new(responses)),
        state(),
        ToolRegistry::new(),
    )
    .unwrap()
}

fn collect(stream: SendBoxStream<'_, AgentEvent>) -> Vec<AgentEvent> {
    block_on(stream.collect())
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
        _ => "unknown",
    }
}

fn structural_names(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .filter(|event| !matches!(event, AgentEvent::AssistantUpdate { .. }))
        .map(event_name)
        .collect()
}

fn text_of(record: &AgentRecord) -> Option<&str> {
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

fn assistant_from(record: &AgentRecord) -> Option<&AssistantMessage> {
    match record {
        AgentRecord::Llm(Message::Assistant(message)) => Some(message),
        AgentRecord::Llm(Message::User(_) | Message::ToolResult(_))
        | AgentRecord::Custom { .. } => None,
    }
}

#[test]
fn agent_prompt_text_event_sequence() {
    // §10.9 Lifecycle. Pi basis: packages/agent/src/agent-loop.ts runAgentLoop
    // and packages/agent/src/agent.ts normalizePromptInput.
    let mut agent = agent([text_response("hello")]);
    let events = collect(agent.prompt_text(
        PromptText {
            text: "prompt".into(),
            images: vec![PromptImage {
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            }],
        },
        CancellationToken::new(),
    ));
    assert_eq!(
        structural_names(&events),
        [
            "run_started",
            "turn_started",
            "message_started_user",
            "message_committed_user",
            "context_prepared",
            "message_started_assistant",
            "message_committed_assistant",
            "turn_finished",
            "run_finished",
        ]
    );
    let AgentRecord::Llm(Message::User(prompt)) = &agent.state().transcript[0] else {
        panic!("expected user prompt");
    };
    assert!(matches!(prompt.content[0], ContentBlock::Text { .. }));
    assert!(matches!(prompt.content[1], ContentBlock::Image { .. }));
}

#[test]
fn agent_run_accepts_agent_input() {
    // Architecture v2 part 1 §4.3 and part 2 §9.3. Pi basis: agentLoop accepts
    // an ordered prompt batch and returns the low-level lifecycle stream.
    let mut agent = agent([text_response("done")]);
    let events = collect(agent.run(
        AgentInput::records([user("user-input", "prompt")]),
        CancellationToken::new(),
    ));
    assert!(matches!(
        events.first(),
        Some(AgentEvent::RunStarted { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished { .. })
    ));
}

#[test]
fn agent_prompt_message_event_sequence() {
    // §10.9 Lifecycle. Pi basis: agent-loop.ts emits start/end for every supplied prompt.
    let mut agent = agent([text_response("done")]);
    let events = collect(agent.prompt_records([user("user-1", "one")], CancellationToken::new()));
    assert_eq!(
        structural_names(&events)[2..4],
        ["message_started_user", "message_committed_user"]
    );
}

#[test]
fn agent_prompt_message_batch_event_sequence() {
    // §10.9 Lifecycle. Pi basis: agent-loop.ts emits prompt batches in source order.
    let mut agent = agent([text_response("done")]);
    let events = collect(agent.prompt_records(
        [user("user-1", "one"), user("user-2", "two")],
        CancellationToken::new(),
    ));
    let committed = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageCommitted { message } => text_of(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(committed, ["one", "two"]);
}

#[test]
fn agent_prompt_without_tools() {
    // §10.9 Lifecycle; part 2 §8.3 prompt() without tools, verbatim structural order.
    agent_prompt_text_event_sequence();
}

#[test]
fn agent_run_finished_is_final_event() {
    // §10.9 Lifecycle. Pi basis: agent_end is the raw loop's terminal event.
    let mut agent = agent([text_response("done")]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished { .. })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::RunFinished { .. }))
            .count(),
        1
    );
}

#[test]
fn agent_low_level_stream_is_observational() {
    // §10.9 Lifecycle. The borrowed low-level stream advances only when polled
    // and has no acknowledged subscriber barrier (part 1 §4.3; part 2 §8.1).
    let runtime = ScriptedRuntime::new([text_response("done")]);
    let mut agent = Agent::new(Arc::new(runtime.clone()), state(), ToolRegistry::new()).unwrap();
    let mut stream = agent.prompt_records([user("user-1", "go")], CancellationToken::new());
    let first = block_on(stream.next()).unwrap();
    assert!(matches!(first, AgentEvent::RunStarted { .. }));
    assert_eq!(runtime.remaining(), 1);
    drop(stream);
    assert!(agent.active_run_id().is_none());
}

#[test]
fn agent_local_run_matches_send_lifecycle() {
    // Architecture v2 part 2 §9.2–§9.3. Pi basis: the same agent-loop.ts
    // lifecycle is exposed through Rust's deliberate local trait family.
    let runtime = Rc::new(ScriptedRuntime::new([text_response("done")]));
    let mut agent = LocalAgent::new(runtime, state(), LocalToolRegistry::new()).unwrap();
    let events = block_on(
        agent
            .prompt_records([user("user-local", "go")], CancellationToken::new())
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        structural_names(&events),
        [
            "run_started",
            "turn_started",
            "message_started_user",
            "message_committed_user",
            "context_prepared",
            "message_started_assistant",
            "message_committed_assistant",
            "turn_finished",
            "run_finished",
        ]
    );
}

#[derive(Clone)]
struct EchoTool {
    spec: ToolSpec,
    log: Arc<Mutex<Vec<String>>>,
    control: Option<AgentControl>,
    steer_after: Option<String>,
}

impl EchoTool {
    fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            spec: ToolSpec {
                schema_version: 1,
                name: "echo".into(),
                description: "echo".into(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            log,
            control: None,
            steer_after: None,
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
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            lock(&self.log).push(context.call.id.as_str().to_owned());
            if self
                .steer_after
                .as_deref()
                .is_some_and(|id| id == context.call.id.as_str())
            {
                self.control
                    .as_ref()
                    .expect("steering tool requires control")
                    .steer(user("steering-from-tool", "steer now"))
                    .await
                    .unwrap();
            }
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new(format!("{}-result", context.call.id.as_str())),
                text: "ok".into(),
            }]))
        })
    }
}

fn tool_agent(
    responses: impl IntoIterator<Item = ScriptedResponse>,
) -> (Agent, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(EchoTool::new(log.clone())))
        .unwrap();
    (
        Agent::new(Arc::new(ScriptedRuntime::new(responses)), state(), tools).unwrap(),
        log,
    )
}

#[test]
fn agent_prompt_with_one_tool() {
    // §10.9 Lifecycle; part 2 §8.3 prompt() with tools.
    let (mut agent, log) = tool_agent([
        tool_call_response("echo", json!({"value":"one"})),
        text_response("done"),
    ]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert_eq!(lock(&log).as_slice(), ["scripted-call-0"]);
    assert_eq!(
        structural_names(&events),
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
}

#[test]
fn agent_run_usage_aggregates_mixed_authoritative_totals_in_both_orders() {
    // Architecture v2 part 1 §3.9 and part 2 §8.1. A run may span
    // responses where one provider reports a nonzero authoritative total and
    // another reports zero. Pi treats the zero as absent, so each response
    // contributes its effective total independently in either order.
    let authoritative = Usage {
        input_tokens: 2,
        output_tokens: 3,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_write_one_hour_tokens: None,
        total_tokens: Some(20),
        source: UsageSource::ProviderReported,
    };
    let component_only = Usage {
        input_tokens: 6,
        output_tokens: 4,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_write_one_hour_tokens: None,
        total_tokens: Some(0),
        source: UsageSource::Estimated,
    };

    for (case, first, second) in [
        (
            "authoritative_then_component",
            authoritative.clone(),
            component_only.clone(),
        ),
        (
            "component_then_authoritative",
            component_only.clone(),
            authoritative.clone(),
        ),
    ] {
        let (mut agent, _) = tool_agent([
            tool_call_response("echo", json!({})).with_usage(first),
            text_response("done").with_usage(second),
        ]);
        let events =
            collect(agent.prompt_records([user("user-usage", "go")], CancellationToken::new()));
        let run_usage = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::RunFinished {
                    outcome: RunOutcome::Completed { usage, .. },
                } => Some(usage),
                _ => None,
            })
            .expect("completed run outcome");

        assert_eq!(run_usage.total_tokens, Some(30), "{case}");
        assert_eq!(run_usage.total_tokens(), 30, "{case}");
        assert_eq!(run_usage.source, UsageSource::Mixed, "{case}");
    }
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
        assembler.apply(event).unwrap();
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
                name: Some("echo".into()),
            },
            AssistantEvent::ToolArgumentsDelta {
                block_id: block_id.clone(),
                delta: format!("{{\"index\":{index}}}"),
            },
            AssistantEvent::ContentBlockFinished { block_id },
        ] {
            assembler.apply(&event).unwrap();
            events.push(event);
        }
    }
    let message = assembler
        .finish_completed(AssistantFinish {
            reason: AssistantFinishReason::ToolUse,
            raw_provider_reason: None,
            error: None,
        })
        .unwrap();
    events.push(AssistantEvent::Finished { message });
    ScriptedResponse::events(events)
}

#[test]
fn agent_prompt_with_multiple_tools() {
    // §10.9 Lifecycle. Pi basis: agent-loop.ts complete batch then source-order results.
    let (mut agent, log) = tool_agent([multiple_tool_response(), text_response("done")]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert_eq!(lock(&log).as_slice(), ["call-0", "call-1"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::ToolResult(_))
                }
            ))
            .count(),
        2
    );
}

#[test]
fn agent_continue_event_sequence() {
    // §10.9 Lifecycle; part 2 §8.3 continue() omits prior user lifecycle events.
    let mut agent = agent([text_response("done")]);
    agent
        .state_mut()
        .unwrap()
        .transcript
        .push(user("user-1", "go"));
    let events = collect(agent.continue_run(CancellationToken::new()).unwrap());
    assert_eq!(
        structural_names(&events),
        [
            "run_started",
            "turn_started",
            "context_prepared",
            "message_started_assistant",
            "message_committed_assistant",
            "turn_finished",
            "run_finished",
        ]
    );
}

#[test]
fn agent_continue_without_tools() {
    // §10.9 Lifecycle; same Pi basis as agent_continue_event_sequence.
    agent_continue_event_sequence();
}

#[test]
fn agent_continue_with_tools() {
    // §10.9 Lifecycle; part 2 §8.3 continue() with resulting tool calls.
    let (mut agent, _) = tool_agent([tool_call_response("echo", json!({})), text_response("done")]);
    agent
        .state_mut()
        .unwrap()
        .transcript
        .push(user("user-1", "go"));
    let events = collect(agent.continue_run(CancellationToken::new()).unwrap());
    assert!(!structural_names(&events).contains(&"message_started_user"));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnStarted { .. }))
            .count(),
        2
    );
}

#[derive(Clone)]
struct RawEventRuntime {
    responses: Arc<Mutex<VecDeque<Vec<AssistantEvent>>>>,
}

impl RawEventRuntime {
    fn new(responses: impl IntoIterator<Item = Vec<AssistantEvent>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }
}

impl ModelRuntime for RawEventRuntime {
    fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>> {
        Box::pin(async move {
            let events = lock(&self.responses).pop_front().ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    "raw event runtime has no remaining response",
                )
            })?;
            Ok(AssistantStream::new(stream::iter(events)))
        })
    }
}

fn text_stream_with_mutated_terminal(
    mutate: impl FnOnce(&mut AssistantMessage),
) -> Vec<AssistantEvent> {
    let block_id = ContentBlockId::new("partial-block");
    let mut assembler = AssistantAssembler::new();
    let mut events = vec![AssistantEvent::MessageStarted {
        message_id: MessageId::new("partial-assistant"),
        provider: ProviderId::new("scripted"),
        api: ApiId::new("scripted"),
        model: ModelId::new("test-model"),
    }];
    for event in [
        AssistantEvent::ContentBlockStarted {
            block_id: block_id.clone(),
            content_index: 0,
            kind: ContentBlockKind::Text,
        },
        AssistantEvent::TextDelta {
            block_id: block_id.clone(),
            delta: "partial".into(),
        },
        AssistantEvent::ContentBlockFinished { block_id },
    ] {
        events.push(event);
    }
    for event in &events {
        assembler.apply(event).unwrap();
    }
    let mut message = assembler
        .finish_completed(AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        })
        .unwrap();
    mutate(&mut message);
    events.push(AssistantEvent::Finished { message });
    events
}

fn terminal_only_message(id: &str, reason: AssistantFinishReason) -> AssistantMessage {
    let provider = ProviderId::new("scripted");
    let api = ApiId::new("scripted");
    let model = ModelId::new("test-model");
    let error = match reason {
        AssistantFinishReason::Error => Some(error()),
        AssistantFinishReason::Aborted => Some(PublicError {
            code: "cancelled".into(),
            message: "Request was aborted".into(),
            retryable: false,
            provider_code: None,
            status: None,
            request_id: None,
        }),
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => None,
    };
    AssistantMessage {
        id: MessageId::new(id),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: model.clone(),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-text")),
            text: "terminal-only partial".into(),
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(provider, api, model.clone(), model)),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error,
        },
        timestamp: Timestamp::default(),
    }
}

fn terminal_only_with_incomplete_replay(id: &str) -> AssistantMessage {
    let mut message = terminal_only_message(id, AssistantFinishReason::Stop);
    message.replay.items.push(ReplayItem {
        id: ReplayItemId::new(format!("{id}-replay")),
        ordinal: 0,
        target: ReplayTarget::Message,
        kind: ReplayKind::new("test.incomplete"),
        applicability: ReplayApplicability::ExactProviderApiModel,
        completeness: ReplayCompleteness::Incomplete,
        payload: OpaquePayload::Utf8("partial-signature".into()),
    });
    message
}

fn incomplete_replay_terminal_stream() -> Vec<AssistantEvent> {
    let mut assembler = AssistantAssembler::new();
    let events = vec![
        AssistantEvent::MessageStarted {
            message_id: MessageId::new("incomplete-replay-assistant"),
            provider: ProviderId::new("scripted"),
            api: ApiId::new("scripted"),
            model: ModelId::new("test-model"),
        },
        AssistantEvent::ReplayItemStarted {
            item_id: ReplayItemId::new("incomplete-replay"),
            ordinal: 0,
            target: ReplayTarget::Message,
            kind: ReplayKind::new("test.incomplete"),
            applicability: ReplayApplicability::ExactProviderApiModel,
        },
    ];
    for event in &events {
        assembler.apply(event).unwrap();
    }
    let snapshot = assembler.snapshot();
    let message = AssistantMessage {
        id: snapshot.id,
        provider: snapshot.provider,
        api: snapshot.api,
        requested_model: snapshot.requested_model,
        response_model: snapshot.response_model,
        response_id: snapshot.response_id,
        deferred: snapshot.deferred,
        end_turn: snapshot.end_turn,
        diagnostics: snapshot.diagnostics,
        content: snapshot.content,
        replay: snapshot.replay,
        usage: snapshot.usage,
        cost: snapshot.cost,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: snapshot.timestamp,
    };
    events
        .into_iter()
        .chain([AssistantEvent::Finished { message }])
        .collect()
}

fn assert_protocol_terminal_commits_failed(
    response: Vec<AssistantEvent>,
) -> (Vec<AgentEvent>, AssistantMessage) {
    let runtime = RawEventRuntime::new([response]);
    let mut agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    let events =
        collect(agent.prompt_records([user("protocol-user", "go")], CancellationToken::new()));
    let message = assistant_from(agent.state().transcript.last().unwrap())
        .unwrap()
        .clone();
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        message
            .finish
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("assistant_stream_protocol")
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            outcome: RunOutcome::Failed { .. }
        })
    ));
    (events, message)
}

#[test]
fn agent_failed_assistant_is_committed() {
    // §10.9 Failure. Pi basis: agent-loop.ts commits the failed terminal
    // assistant before turn_end and agent_end.
    let mut agent = agent([ScriptedResponse::failure(error())]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    let commit = events.iter().position(|event| matches!(event, AgentEvent::MessageCommitted { message: AgentRecord::Llm(Message::Assistant(message)) } if message.finish.reason == AssistantFinishReason::Error)).unwrap();
    let turn = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnFinished { .. }))
        .unwrap();
    let run = events
        .iter()
        .position(|event| matches!(event, AgentEvent::RunFinished { .. }))
        .unwrap();
    assert!(commit < turn && turn < run);
    assert_eq!(
        assistant_from(agent.state().transcript.last().unwrap())
            .unwrap()
            .finish
            .reason,
        AssistantFinishReason::Error
    );

    // Architecture v2 part 2 §1.3/R2 and §2.1: once a stream starts, the
    // terminal event must agree with stable identity and assembled content,
    // and a successful terminal cannot carry incomplete replay state. Each
    // protocol mismatch becomes a committed failed assistant rather than the
    // supplied successful message.
    let (identity_events, identity_failure) =
        assert_protocol_terminal_commits_failed(text_stream_with_mutated_terminal(|message| {
            message.id = MessageId::new("changed-assistant");
        }));
    assert_eq!(identity_failure.id, MessageId::new("partial-assistant"));
    assert!(identity_events.iter().all(|event| !matches!(
        event,
        AgentEvent::AssistantUpdate {
            event: AssistantEvent::Finished { .. },
            ..
        }
    )));

    let (_, content_failure) =
        assert_protocol_terminal_commits_failed(text_stream_with_mutated_terminal(|message| {
            let ContentBlock::Text { text, .. } = &mut message.content[0] else {
                panic!("text fixture must contain text");
            };
            *text = "tampered".into();
        }));
    assert!(matches!(
        &content_failure.content[0],
        ContentBlock::Text { text, .. } if text == "partial"
    ));

    let (_, replay_failure) =
        assert_protocol_terminal_commits_failed(incomplete_replay_terminal_stream());
    assert!(matches!(
        replay_failure.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    ));

    // Pi permits a final result without a preceding start event. Rust keeps
    // that compatibility, but part 2 §1.3/R2 and §2.1 still require the event
    // variant, finish metadata, and successful replay completeness to agree.
    let terminal_only_cases = [
        AssistantEvent::Failed {
            message: terminal_only_message(
                "terminal-only-failed-success",
                AssistantFinishReason::Stop,
            ),
        },
        AssistantEvent::Finished {
            message: terminal_only_message(
                "terminal-only-finished-error",
                AssistantFinishReason::Error,
            ),
        },
        AssistantEvent::Finished {
            message: terminal_only_message(
                "terminal-only-finished-aborted",
                AssistantFinishReason::Aborted,
            ),
        },
        AssistantEvent::Finished {
            message: terminal_only_with_incomplete_replay(
                "terminal-only-finished-incomplete-replay",
            ),
        },
    ];
    for malformed in terminal_only_cases {
        let malformed_id = malformed.terminal_message().unwrap().id.clone();
        let (events, failure) = assert_protocol_terminal_commits_failed(vec![malformed]);
        assert_eq!(failure.id, malformed_id);
        assert!(matches!(
            &failure.content[0],
            ContentBlock::Text { text, .. } if text == "terminal-only partial"
        ));
        assert!(events.iter().all(|event| !matches!(
            event,
            AgentEvent::AssistantUpdate {
                event: AssistantEvent::Finished { .. }
                    | AssistantEvent::Failed { .. }
                    | AssistantEvent::Cancelled { .. },
                ..
            }
        )));
    }

    let mut invalid_cancellation = terminal_only_message(
        "terminal-only-invalid-cancel",
        AssistantFinishReason::Aborted,
    );
    invalid_cancellation
        .finish
        .error
        .as_mut()
        .unwrap()
        .retryable = true;
    assert_protocol_terminal_commits_failed(vec![AssistantEvent::Cancelled {
        message: invalid_cancellation,
    }]);
}

#[test]
fn agent_failed_turn_has_turn_finished() {
    // §10.9 Failure. Pi basis: agent-loop.ts emits turn_end for error terminal.
    let mut agent = agent([ScriptedResponse::failure(error())]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::TurnFinished { outcome } if outcome.assistant_finish == AssistantFinishReason::Error)));
}

#[test]
fn agent_failed_turn_has_run_finished() {
    // §10.9 Failure. Pi basis: agent-loop.ts emits agent_end after failed turn.
    let mut agent = agent([ScriptedResponse::failure(error())]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            outcome: RunOutcome::Failed { .. }
        })
    ));
}

#[test]
fn agent_partial_content_survives_failure() {
    // §10.9 Failure. Pi basis: agent-loop.ts replaces the streaming partial
    // with the failed terminal assistant without discarding completed content.
    let mut agent = agent([text_response("partial").failing(error())]);
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    let assistant = assistant_from(agent.state().transcript.last().unwrap()).unwrap();
    assert!(matches!(&assistant.content[0], ContentBlock::Text { text, .. } if text == "partial"));
}

#[test]
fn agent_partial_usage_survives_failure() {
    // §10.9 Failure. Pi basis: agent-loop.ts commits the stream result, whose
    // terminal message retains the last cumulative usage observation.
    let expected = usage(11, 7);
    let mut agent = agent([text_response("partial")
        .with_usage(expected.clone())
        .failing(error())]);
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert_eq!(
        assistant_from(agent.state().transcript.last().unwrap())
            .unwrap()
            .usage,
        expected
    );
}

#[test]
fn agent_no_tools_execute_after_failed_assistant() {
    // §10.9 Failure. Pi basis: agent-loop.ts returns before tool filtering on error/aborted.
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(EchoTool::new(log.clone())))
        .unwrap();
    let runtime = ScriptedRuntime::new([tool_call_response("echo", json!({})).failing(error())]);
    let mut agent = Agent::new(Arc::new(runtime), state(), tools).unwrap();
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert!(lock(&log).is_empty());
}

#[test]
fn agent_cancelled_assistant_is_committed() {
    // §10.9 Failure. Pi basis: agent-loop.ts commits an aborted terminal
    // assistant through message_end before turn_end and agent_end.
    let mut agent = agent([text_response("partial")]);
    let control = agent.control();
    let mut stream = agent.prompt_records([user("user-1", "go")], CancellationToken::new());
    let mut events = Vec::new();
    let run_id = loop {
        let event = block_on(stream.next()).unwrap();
        if let AgentEvent::RunStarted { run_id } = &event {
            events.push(event.clone());
            break run_id.clone();
        }
        events.push(event);
    };
    loop {
        let event = block_on(stream.next()).unwrap();
        let is_text = matches!(
            &event,
            AgentEvent::AssistantUpdate {
                event: AssistantEvent::TextDelta { .. },
                ..
            }
        );
        events.push(event);
        if is_text {
            break;
        }
    }
    control.cancel(run_id).unwrap();
    events.extend(block_on(stream.collect::<Vec<_>>()));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::MessageCommitted { message: AgentRecord::Llm(Message::Assistant(message)) } if message.finish.reason == AssistantFinishReason::Aborted)));
    drop(events);
    assert_eq!(
        assistant_from(agent.state().transcript.last().unwrap())
            .unwrap()
            .finish
            .reason,
        AssistantFinishReason::Aborted
    );
}

#[test]
fn agent_continue_rejects_assistant_tail() {
    // §10.9 Failure. Pi basis: agent.ts continue() checks assistant before low-level loop.
    let mut agent = agent([text_response("done")]);
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert!(matches!(
        agent.continue_run(CancellationToken::new()),
        Err(AgentError::ContinueFromAssistant)
    ));
}

#[test]
fn agent_continue_drains_steering_before_rejecting_assistant_tail() {
    // §10.9 Failure. Pi basis: agent.ts continue() drains steering before rejecting assistant tail.
    let mut agent = agent([text_response("first"), text_response("steered")]);
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    block_on(agent.control().steer(user("steer-1", "steer"))).unwrap();
    collect(agent.continue_run(CancellationToken::new()).unwrap());
    assert!(
        agent
            .state()
            .transcript
            .iter()
            .any(|record| text_of(record) == Some("steer"))
    );
}

#[test]
fn agent_continue_drains_followup_after_steering() {
    // §10.9 Failure. Pi basis: agent.ts checks follow-up only after steering is empty.
    let mut agent = agent([
        text_response("first"),
        text_response("steered"),
        text_response("followed"),
    ]);
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    block_on(agent.control().steer(user("steer-1", "steer"))).unwrap();
    block_on(agent.control().follow_up(user("follow-1", "follow"))).unwrap();
    collect(agent.continue_run(CancellationToken::new()).unwrap());
    let steering = agent
        .state()
        .transcript
        .iter()
        .position(|record| text_of(record) == Some("steer"))
        .unwrap();
    let follow_up = agent
        .state()
        .transcript
        .iter()
        .position(|record| text_of(record) == Some("follow"))
        .unwrap();
    assert!(steering < follow_up);
}

#[test]
fn agent_request_start_failure_commits_empty_assistant() {
    // Part 2 §2.1. Pi basis: agent.ts handleRunFailure emits and commits an
    // empty assistant before turn_end/agent_end when stream setup throws.
    let mut agent = agent([]);
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    let assistant = assistant_from(agent.state().transcript.last().unwrap()).unwrap();
    assert_eq!(assistant.finish.reason, AssistantFinishReason::Error);
    assert!(assistant.content.is_empty());
    let committed = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::MessageCommitted {
                    message: AgentRecord::Llm(Message::Assistant(_))
                }
            )
        })
        .unwrap();
    let finished = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnFinished { .. }))
        .unwrap();
    assert!(committed < finished);
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

#[test]
fn agent_retry_last_turn_reuses_last_valid_request_boundary() {
    // §10.9 Failure; deliberate §10.11 retry addition. Failed record remains durable.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([
            text_response("partial").failing(error()),
            text_response("retried"),
        ]),
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    collect(agent.retry_last_turn(CancellationToken::new()).unwrap());
    let requests = lock(&requests);
    assert_eq!(requests[0].context.messages, requests[1].context.messages);
    assert!(agent.state().transcript.iter().any(|record| {
        assistant_from(record)
            .is_some_and(|message| message.finish.reason == AssistantFinishReason::Error)
    }));
}

#[test]
fn queue_steering_polled_at_run_start() {
    // §10.9 Queues; part 2 §8.2 correction. Pi basis: agent-loop.ts
    // runAgentLoop emits turn/prompt lifecycle at lines 109-115 before runLoop's
    // initial getSteeringMessages poll at line 166.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = RecordingRuntime {
        inner: ScriptedRuntime::new([text_response("done")]),
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    let control = agent.control();
    let mut stream = agent.prompt_records([user("user-1", "prompt")], CancellationToken::new());

    let mut saw_turn_start = false;
    loop {
        let event = block_on(stream.next()).expect("prompt lifecycle must precede request");
        saw_turn_start |= matches!(event, AgentEvent::TurnStarted { .. });
        if matches!(
            event,
            AgentEvent::MessageCommitted { ref message }
                if text_of(message) == Some("prompt")
        ) {
            break;
        }
    }
    assert!(saw_turn_start);

    block_on(control.steer(user("steer-1", "steer"))).unwrap();
    block_on(stream.collect::<Vec<_>>());

    let requests = lock(&requests);
    assert_eq!(requests.len(), 1);
    let texts = requests[0]
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(message) => message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                ContentBlock::Image { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::ToolCall { .. } => None,
            }),
            Message::Assistant(_) | Message::ToolResult(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["prompt", "steer"]);
}

#[test]
fn queue_steering_not_polled_between_tools() {
    // §10.9 Queues; part 2 §8.2 critical order. Pi basis: agent-loop.ts polls only after turn_end/policies.
    let log = Arc::new(Mutex::new(Vec::new()));
    let control_slot = Arc::new(Mutex::new(None::<AgentControl>));
    let tool = SteeringTool {
        spec: ToolSpec {
            schema_version: 1,
            name: "echo".into(),
            description: "echo".into(),
            parameters: json!({"type":"object"}),
            constrained_sampling: None,
        },
        log: log.clone(),
        control: control_slot.clone(),
    };
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(tool)).unwrap();
    let mut agent = Agent::new(
        Arc::new(ScriptedRuntime::new([
            multiple_tool_response(),
            text_response("done"),
        ])),
        state(),
        registry,
    )
    .unwrap();
    *lock(&control_slot) = Some(agent.control());
    let events = collect(agent.prompt_records([user("user-1", "go")], CancellationToken::new()));
    assert_eq!(lock(&log).as_slice(), ["call-0", "call-1"]);
    let steering_commit = events.iter().position(|event| matches!(event, AgentEvent::MessageCommitted { message } if text_of(message) == Some("steer now"))).unwrap();
    let second_tool_finish = events
        .iter()
        .rposition(|event| matches!(event, AgentEvent::ToolExecutionFinished { .. }))
        .unwrap();
    let first_turn_finish = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnFinished { .. }))
        .unwrap();
    assert!(second_tool_finish < first_turn_finish && first_turn_finish < steering_commit);
}

#[test]
fn queue_steering_polled_after_completed_turn() {
    // §10.9 Queues. Pi basis: agent-loop.ts polls steering only after turn_end.
    // The stronger two-tool assertion also proves this positive boundary.
    queue_steering_not_polled_between_tools();
}

struct QueueingTurnPolicy {
    control: AgentControl,
    enqueued: AtomicBool,
    stop: bool,
}

struct FailingShouldStopPolicy;

impl TurnPolicy for FailingShouldStopPolicy {
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
        Box::pin(async { Err(TurnPolicyError::new("stop policy failed")) })
    }
}

#[test]
fn agent_should_stop_failure_commits_assistant() {
    // Part 2 §2.1 and §8.2. Pi basis: a rejected shouldStopAfterTurn escapes
    // runLoop and agent.ts handleRunFailure commits a failed assistant before
    // the final turn_end and agent_end.
    let mut agent = agent([text_response("first")]);
    agent
        .set_turn_policy(Arc::new(FailingShouldStopPolicy))
        .unwrap();
    let events = collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    let failed = assistant_from(agent.state().transcript.last().unwrap()).unwrap();
    assert_eq!(failed.finish.reason, AssistantFinishReason::Error);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            outcome: RunOutcome::Failed { .. }
        })
    ));
}

impl TurnPolicy for QueueingTurnPolicy {
    fn prepare_next_turn<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<NextTurn, TurnPolicyError>> {
        Box::pin(async move {
            if !self.enqueued.swap(true, Ordering::AcqRel) {
                self.control
                    .steer(user("policy-steer", "policy steer"))
                    .await
                    .unwrap();
            }
            Ok(NextTurn::default())
        })
    }

    fn should_stop<'a>(
        &'a self,
        _turn: CompletedTurn<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<bool, TurnPolicyError>> {
        Box::pin(async move { Ok(self.stop) })
    }
}

#[test]
fn queue_steering_polled_after_prepare_next_turn() {
    // §10.9 Queues; part 2 §8.2. Pi basis: prepareNextTurn precedes getSteeringMessages.
    let mut agent = agent([text_response("first"), text_response("second")]);
    agent
        .set_turn_policy(Arc::new(QueueingTurnPolicy {
            control: agent.control(),
            enqueued: AtomicBool::new(false),
            stop: false,
        }))
        .unwrap();
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert!(
        agent
            .state()
            .transcript
            .iter()
            .any(|record| text_of(record) == Some("policy steer"))
    );
}

#[test]
fn queue_steering_not_polled_when_should_stop_returns_true() {
    // §10.9 Queues. Pi basis: shouldStopAfterTurn exits before either queue poll.
    let mut agent = agent([text_response("first")]);
    agent
        .set_turn_policy(Arc::new(QueueingTurnPolicy {
            control: agent.control(),
            enqueued: AtomicBool::new(false),
            stop: true,
        }))
        .unwrap();
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert!(
        !agent
            .state()
            .transcript
            .iter()
            .any(|record| text_of(record) == Some("policy steer"))
    );
    assert_eq!(agent.clear_steering_queue(), 1);
}

#[test]
fn queue_followup_polled_only_when_agent_would_stop() {
    // §10.9 Queues. Pi basis: agent-loop.ts checks follow-up outside the
    // tool/steering inner loop, after the final no-tool assistant.
    let (mut agent, _) = tool_agent([
        tool_call_response("echo", json!({})),
        text_response("after tool"),
        text_response("after follow-up"),
    ]);
    block_on(agent.control().follow_up(user("f", "follow"))).unwrap();
    let events = collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnStarted { .. }))
            .count(),
        3
    );
    let follow_commit = events
        .iter()
        .position(|event| matches!(event, AgentEvent::MessageCommitted { message } if text_of(message) == Some("follow")))
        .unwrap();
    let first_turn = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnFinished { .. }))
        .unwrap();
    assert!(first_turn < follow_commit);
}

struct SteeringTool {
    spec: ToolSpec,
    log: Arc<Mutex<Vec<String>>>,
    control: Arc<Mutex<Option<AgentControl>>>,
}

impl Tool for SteeringTool {
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
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            lock(&self.log).push(context.call.id.as_str().to_owned());
            if context.call.id.as_str() == "call-0" {
                let control = lock(&self.control).clone().unwrap();
                control
                    .steer(user("steer-tool", "steer now"))
                    .await
                    .unwrap();
            }
            Ok(ToolOutput::new(Vec::new()))
        })
    }
}

#[test]
fn queue_one_mode_drains_one() {
    // §10.9 Queues. Pi basis: PendingMessageQueue one-at-a-time drain.
    let mut agent = agent([text_response("one"), text_response("two")]);
    let control = agent.control();
    block_on(control.steer(user("steer-1", "one"))).unwrap();
    block_on(control.steer(user("steer-2", "two"))).unwrap();
    collect(agent.prompt_records([user("user-1", "prompt")], CancellationToken::new()));
    let texts = agent
        .state()
        .transcript
        .iter()
        .filter_map(text_of)
        .collect::<Vec<_>>();
    assert_eq!(texts, ["prompt", "one", "two"]);
}

#[test]
fn queue_all_mode_drains_all() {
    // §10.9 Queues. Pi basis: PendingMessageQueue all drain.
    let mut agent = agent([text_response("done")]);
    agent.set_steering_mode(QueueDrainMode::All);
    let control = agent.control();
    block_on(control.steer(user("steer-1", "one"))).unwrap();
    block_on(control.steer(user("steer-2", "two"))).unwrap();
    collect(agent.prompt_records([user("user-1", "prompt")], CancellationToken::new()));
    let texts = agent
        .state()
        .transcript
        .iter()
        .filter_map(text_of)
        .collect::<Vec<_>>();
    assert_eq!(texts, ["prompt", "one", "two"]);
    assert_eq!(
        agent
            .state()
            .transcript
            .iter()
            .filter(|record| assistant_from(record).is_some())
            .count(),
        1
    );
}

#[test]
fn queue_ingress_order_is_stable() {
    // §10.9 Queues; part 2 §8.4 QueueCommand monotonic ingress sequence.
    let mut agent = agent([text_response("done")]);
    agent.set_steering_mode(QueueDrainMode::All);
    let control = agent.control();
    let a = block_on(control.steer(user("a", "a"))).unwrap();
    let b = block_on(control.steer(user("b", "b"))).unwrap();
    assert!(a.sequence < b.sequence);
    collect(agent.prompt_records([user("prompt", "prompt")], CancellationToken::new()));
    assert_eq!(
        agent
            .state()
            .transcript
            .iter()
            .filter_map(text_of)
            .collect::<Vec<_>>(),
        ["prompt", "a", "b"]
    );
}

#[test]
fn queue_clear_steering() {
    // §10.9 Queues. Pi basis: Agent.clearSteeringQueue.
    let mut agent = agent([text_response("done")]);
    block_on(agent.control().steer(user("s", "steer"))).unwrap();
    assert_eq!(agent.clear_steering_queue(), 1);
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert!(
        !agent
            .state()
            .transcript
            .iter()
            .any(|record| text_of(record) == Some("steer"))
    );
}

#[test]
fn queue_clear_followup() {
    // §10.9 Queues. Pi basis: Agent.clearFollowUpQueue.
    let mut agent = agent([text_response("done")]);
    block_on(agent.control().follow_up(user("f", "follow"))).unwrap();
    assert_eq!(agent.clear_follow_up_queue(), 1);
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert!(
        !agent
            .state()
            .transcript
            .iter()
            .any(|record| text_of(record) == Some("follow"))
    );
}

#[test]
fn queue_clear_all() {
    // §10.9 Queues. Pi basis: Agent.clearAllQueues.
    let agent = agent([]);
    let control = agent.control();
    block_on(control.steer(user("s", "steer"))).unwrap();
    block_on(control.follow_up(user("f", "follow"))).unwrap();
    assert_eq!(agent.clear_all_queues(), 2);
}

#[test]
fn queue_concurrent_producers_use_control_handle() {
    // §10.9 Queues; part 2 §8.4 cloneable control handle.
    let agent = agent([]);
    let control = agent.control();
    let threads = (0..8)
        .map(|index| {
            let control = control.clone();
            std::thread::spawn(move || {
                block_on(control.steer(user(&format!("u-{index}"), "queued")))
                    .unwrap()
                    .sequence
            })
        })
        .collect::<Vec<_>>();
    let mut sequences = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    sequences.sort();
    sequences.dedup();
    assert_eq!(sequences.len(), 8);
}

#[test]
fn agent_reset_rejects_while_active() {
    // §10.9 State management. Pi basis: agent.ts reset rejects an active run.
    let mut agent = agent([text_response("done")]);
    let stream = agent.prompt_records([user("p", "prompt")], CancellationToken::new());
    std::mem::forget(stream);
    assert!(matches!(
        agent.reset_transcript(),
        Err(AgentError::RunActive)
    ));
}

#[test]
fn agent_reset_clears_transcript() {
    // §10.9 State management. Pi basis: agent.ts reset clears messages.
    let mut agent = agent([text_response("done")]);
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    agent.reset_transcript().unwrap();
    assert!(agent.state().transcript.is_empty());
}

#[test]
fn agent_reset_clears_partial_state() {
    // §10.9 State management. Pi basis: reset clears streamingMessage.
    let mut agent = agent([text_response("done")]);
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    agent.reset_transcript().unwrap();
    assert!(agent.snapshot().streaming.is_none());
}

#[test]
fn agent_reset_clears_pending_tool_calls() {
    // §10.9 State management. Pi basis: reset replaces pendingToolCalls with an empty set.
    let mut agent = agent([text_response("done")]);
    agent.reset_transcript().unwrap();
    assert!(agent.snapshot().pending_tool_calls.is_empty());
}

#[test]
fn agent_reset_clears_error() {
    // §10.9 State management. Pi basis: reset clears errorMessage.
    let mut agent = agent([ScriptedResponse::failure(error())]);
    collect(agent.prompt_records([user("p", "prompt")], CancellationToken::new()));
    assert!(agent.last_error().is_some());
    agent.reset_transcript().unwrap();
    assert!(agent.last_error().is_none());
}

#[test]
fn agent_reset_clears_queues() {
    // §10.9 State management. Pi basis: reset clears both queues.
    let mut agent = agent([]);
    block_on(agent.control().steer(user("s", "steer"))).unwrap();
    block_on(agent.control().follow_up(user("f", "follow"))).unwrap();
    agent.reset_transcript().unwrap();
    assert_eq!(agent.clear_all_queues(), 0);
}

#[test]
fn agent_reset_preserves_model() {
    // §10.9 State management. Pi basis: reset retains configured model.
    let mut agent = agent([]);
    let model = agent.state().model.clone();
    agent.reset_transcript().unwrap();
    assert_eq!(agent.state().model, model);
}

#[test]
fn agent_reset_preserves_system_prompt() {
    // §10.9 State management. Pi basis: reset retains systemPrompt.
    let mut agent = agent([]);
    let prompt = agent.state().system_prompt.clone();
    agent.reset_transcript().unwrap();
    assert_eq!(agent.state().system_prompt, prompt);
}

#[test]
fn agent_reset_preserves_tools() {
    // §10.9 State management. Pi basis: reset retains tools.
    let (mut agent, _) = tool_agent([]);
    agent.reset_transcript().unwrap();
    assert_eq!(agent.tools().len(), 1);
}

#[test]
fn agent_reset_preserves_runtime_and_policies() {
    // §10.9 State management. Pi basis: reset retains callbacks/options and stream function.
    let runtime = Arc::new(ScriptedRuntime::new([text_response("done")]));
    let runtime_dyn: Arc<dyn ModelRuntime> = runtime.clone();
    let mut agent = Agent::new(runtime_dyn.clone(), state(), ToolRegistry::new()).unwrap();
    agent.options_mut().unwrap().session_id = Some("session".into());
    agent.reset_transcript().unwrap();
    assert!(Arc::ptr_eq(agent.runtime(), &runtime_dyn));
    assert_eq!(agent.options().session_id.as_deref(), Some("session"));
}

#[test]
fn agent_reset_all_restores_builder_defaults() {
    // Part 2 §8.1. reset_all is the native companion to Pi-compatible
    // reset_transcript and restores the values captured at construction.
    let mut agent = agent([]);
    let original = agent.state().clone();
    {
        let state = agent.state_mut().unwrap();
        state.system_prompt = "changed".into();
        state.model = ModelRef::new("other", "other-model");
        state.reasoning = ReasoningLevel::High;
        state.transcript.push(user("p", "prompt"));
    }
    agent.options_mut().unwrap().session_id = Some("changed-session".into());
    agent.set_steering_mode(QueueDrainMode::All);
    agent.set_follow_up_mode(QueueDrainMode::All);

    agent.reset_all().unwrap();

    assert_eq!(agent.state().system_prompt, original.system_prompt);
    assert_eq!(agent.state().model, original.model);
    assert_eq!(agent.state().reasoning, original.reasoning);
    assert!(agent.state().transcript.is_empty());
    assert_eq!(agent.options(), &SimpleGenerationOptions::default());
    assert_eq!(agent.steering_mode(), QueueDrainMode::One);
    assert_eq!(agent.follow_up_mode(), QueueDrainMode::One);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
