//! Case-by-case port of `pi/packages/agent/test/agent-loop.test.ts`.
//!
//! All 21 complete executable contracts were enabled and green at the M1-M6 checkpoint; the two
//! blocked-call termination cases landed upstream afterwards and are ported here too.

#![cfg(feature = "testing")]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, ChatOptions, ChatRole, ReasoningEffort};
use genai::{ModelIden, ModelSpec};
use rust_genai_agent::testing::{EventRecorder, MockStreamFn, ScriptedStream};
use rust_genai_agent::{
    AfterToolCallResult, AgentContext, AgentError, AgentEvent, AgentLoopConfig,
    AgentLoopTurnUpdate, AgentMessage, AgentTool, AgentToolCall, AgentToolResult, AgentUsage,
    AssistantContent, AssistantMessage, AssistantMessageEvent, BeforeToolCallResult, CustomMessage,
    EventKind, FnTool, LoopError, StopReason, ThinkingBudgets, ThinkingLevel, ToolCallContext,
    ToolError, ToolExecutionMode, ToolHookError, ToolResultContent, ToolResultMessage, ToolSpec,
    TryAfterToolCallHook, TryBeforeToolCallHook, UpdateSink, default_convert_to_llm,
    run_agent_loop, run_agent_loop_continue, set_default_stream_fn,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn model_iden() -> ModelIden {
    ModelIden::new(AdapterKind::OpenAIResp, "mock")
}

fn model() -> ModelSpec {
    ModelSpec::from_iden(model_iden())
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::user(text)
}

fn text_response(text: &str) -> AssistantMessage {
    AssistantMessage::completed(
        model_iden(),
        vec![AssistantContent::text(text)],
        StopReason::Stop,
    )
}

fn tool_response(calls: Vec<AgentToolCall>, reason: StopReason) -> AssistantMessage {
    AssistantMessage::completed(
        model_iden(),
        calls.into_iter().map(AssistantContent::tool_call).collect(),
        reason,
    )
}

fn call(id: &str, name: &str, args: Value) -> AgentToolCall {
    AgentToolCall::new(id, name, args)
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn value_schema() -> Value {
    schema(json!({ "value": { "type": "string" } }), &["value"])
}

fn tool_spec(name: &str, schema: Value) -> ToolSpec {
    ToolSpec::new(name, format!("{name} tool"), schema).with_label(name)
}

fn base_config() -> AgentLoopConfig {
    AgentLoopConfig::new(model(), default_convert_to_llm())
}

fn empty_context(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext {
    AgentContext::new("").with_tools(tools)
}

fn mock(responses: Vec<AssistantMessage>) -> Arc<MockStreamFn> {
    Arc::new(MockStreamFn::from_streams(
        responses
            .into_iter()
            .map(|message| {
                let reason = message.stop_reason;
                ScriptedStream::from_events(vec![AssistantMessageEvent::Done { reason, message }])
            })
            .collect(),
    ))
}

async fn run_new(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    stream: Option<Arc<dyn rust_genai_agent::StreamFn>>,
) -> (Vec<AgentMessage>, Vec<AgentEvent>) {
    let mut recorder = EventRecorder::new();
    let messages = run_agent_loop(
        prompts,
        context,
        config,
        &mut recorder,
        CancellationToken::new(),
        stream,
    )
    .await
    .expect("the low-level loop invocation should be valid");
    (messages, recorder.events())
}

async fn run_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    stream: Option<Arc<dyn rust_genai_agent::StreamFn>>,
) -> Result<(Vec<AgentMessage>, Vec<AgentEvent>), LoopError> {
    let mut recorder = EventRecorder::new();
    let messages = run_agent_loop_continue(
        context,
        config,
        &mut recorder,
        CancellationToken::new(),
        stream,
    )
    .await?;
    Ok((messages, recorder.events()))
}

fn roles(messages: &[AgentMessage]) -> Vec<&str> {
    messages.iter().map(AgentMessage::role).collect()
}

fn event_kinds(events: &[AgentEvent]) -> Vec<EventKind> {
    events.iter().map(AgentEvent::kind).collect()
}

fn tool_result_ids_from_message_end(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
            } => Some(result.tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_end_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn turn_tool_result_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } => tool_results
                .iter()
                .map(|result| result.tool_call_id.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

fn first_text(content: &[ToolResultContent]) -> &str {
    content
        .iter()
        .find_map(|part| match part {
            ToolResultContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn user_text(message: &AgentMessage) -> Option<&str> {
    match message {
        AgentMessage::User(message) => message.content.iter().find_map(|part| match part {
            rust_genai_agent::UserContent::Text { text } => Some(text.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("uses the configured default when a legacy caller omits streamFn")
#[tokio::test]
async fn uses_configured_default_when_stream_fn_is_none() {
    let fallback = mock(vec![text_response("fallback")]);
    set_default_stream_fn(Some(fallback.clone()));

    let result = run_new(
        vec![user("Hello")],
        empty_context(Vec::new()),
        base_config(),
        None,
    )
    .await;
    set_default_stream_fn(None);

    let (_messages, _events) = result;
    assert_eq!(fallback.calls().len(), 1);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should emit events with AgentMessage types")
#[tokio::test]
async fn emits_events_with_agent_message_types() {
    let (messages, events) = run_new(
        vec![user("Hello")],
        AgentContext::new("You are helpful."),
        base_config(),
        Some(mock(vec![text_response("Hi there!")])),
    )
    .await;

    assert_eq!(roles(&messages), ["user", "assistant"]);
    let kinds = event_kinds(&events);
    for expected in [
        EventKind::AgentStart,
        EventKind::TurnStart,
        EventKind::MessageStart,
        EventKind::MessageEnd,
        EventKind::TurnEnd,
        EventKind::AgentEnd,
    ] {
        assert!(kinds.contains(&expected), "missing event kind {expected:?}");
    }
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should handle custom message types via convertToLlm")
#[tokio::test]
async fn handles_custom_message_types_via_convert_to_llm() {
    let converted = Arc::new(Mutex::new(Vec::<ChatMessage>::new()));
    let converted_for_hook = converted.clone();
    let mut config = base_config();
    config.convert_to_llm = Arc::new(move |messages| {
        let converted = converted_for_hook.clone();
        Box::pin(async move {
            let standard = messages
                .into_iter()
                .filter(|message| !matches!(message, AgentMessage::Custom(_)))
                .collect::<Vec<_>>();
            let llm = rust_genai_agent::convert_messages_to_llm(&standard);
            *converted.lock().unwrap() = llm.clone();
            llm
        })
    });
    let context = AgentContext::new("You are helpful.").with_messages(vec![AgentMessage::Custom(
        CustomMessage::new("notification", json!({ "text": "This is a notification" })),
    )]);

    run_new(
        vec![user("Hello")],
        context,
        config,
        Some(mock(vec![text_response("Response")])),
    )
    .await;

    let converted = converted.lock().unwrap();
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].role, ChatRole::User);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should apply transformContext before convertToLlm")
#[tokio::test]
async fn applies_transform_context_before_convert_to_llm() {
    let transformed = Arc::new(Mutex::new(Vec::<AgentMessage>::new()));
    let converted = Arc::new(Mutex::new(Vec::<ChatMessage>::new()));
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let mut config = base_config();
    let transformed_for_hook = transformed.clone();
    let transform_order = order.clone();
    config.transform_context = Some(Arc::new(move |messages, _cancel| {
        let transformed = transformed_for_hook.clone();
        let order = transform_order.clone();
        Box::pin(async move {
            order.lock().unwrap().push("transform");
            let pruned = messages.into_iter().rev().take(2).collect::<Vec<_>>();
            let pruned = pruned.into_iter().rev().collect::<Vec<_>>();
            *transformed.lock().unwrap() = pruned.clone();
            pruned
        })
    }));
    let converted_for_hook = converted.clone();
    let convert_order = order.clone();
    config.convert_to_llm = Arc::new(move |messages| {
        let converted = converted_for_hook.clone();
        let order = convert_order.clone();
        Box::pin(async move {
            order.lock().unwrap().push("convert");
            let llm = rust_genai_agent::convert_messages_to_llm(&messages);
            *converted.lock().unwrap() = llm.clone();
            llm
        })
    });

    let context = AgentContext::new("You are helpful.").with_messages(vec![
        user("old message 1"),
        AgentMessage::assistant(text_response("old response 1")),
        user("old message 2"),
        AgentMessage::assistant(text_response("old response 2")),
    ]);
    run_new(
        vec![user("new message")],
        context,
        config,
        Some(mock(vec![text_response("Response")])),
    )
    .await;

    assert_eq!(transformed.lock().unwrap().len(), 2);
    assert_eq!(converted.lock().unwrap().len(), 2);
    assert_eq!(*order.lock().unwrap(), ["transform", "convert"]);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should handle tool calls and results")
#[tokio::test]
async fn handles_tool_calls_and_results() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_by_tool = executed.clone();
    let tool_usage = AgentUsage::new(1, 2)
        .with_cache_read_tokens(3)
        .with_cache_write_tokens(4)
        .with_total_tokens(10);
    let patched_usage = AgentUsage::new(5, 6)
        .with_cache_read_tokens(7)
        .with_cache_write_tokens(8)
        .with_total_tokens(26);
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("echoed: {value}"))],
                    json!({ "value": value }),
                )
                .with_usage(tool_usage))
            }
        },
    ));
    let observed_usage = Arc::new(Mutex::new(None::<AgentUsage>));
    let observed_by_hook = observed_usage.clone();
    let mut config = base_config();
    config.after_tool_call = Some(Arc::new(move |hook, _cancel| {
        let observed = observed_by_hook.clone();
        Box::pin(async move {
            *observed.lock().unwrap() = hook.result.usage;
            Some(AfterToolCallResult {
                usage: Some(patched_usage),
                ..AfterToolCallResult::default()
            })
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(stream),
    )
    .await;

    assert_eq!(*executed.lock().unwrap(), ["hello"]);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
    );
    let tool_end = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .expect("tool_execution_end");
    assert!(!tool_end);
    assert_eq!(*observed_usage.lock().unwrap(), Some(tool_usage));
    let result_usage = messages.iter().find_map(|message| match message {
        AgentMessage::ToolResult(result) => result.usage,
        _ => None,
    });
    assert_eq!(result_usage, Some(patched_usage));
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should not execute tool calls from a length-truncated assistant message")
#[tokio::test]
async fn does_not_execute_tool_calls_from_length_truncated_message() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("echoed: {value}"))],
                    json!({ "value": value }),
                ))
            }
        },
    ));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hel" }))],
            StopReason::Length,
        ),
        text_response("done"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        base_config(),
        Some(stream.clone()),
    )
    .await;

    assert!(executed.lock().unwrap().is_empty());
    let (is_error, error_text) = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((*is_error, first_text(&result.content).to_owned())),
            _ => None,
        })
        .expect("tool_execution_end");
    assert!(is_error);
    assert!(error_text.contains("output token limit"));
    assert_eq!(stream.calls().len(), 2, "the model may re-issue the call");
    assert_eq!(messages.last().unwrap().role(), "assistant");
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should execute mutated beforeToolCall args without revalidation")
#[tokio::test]
async fn executes_mutated_before_tool_call_args_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                executed.lock().unwrap().push(args["value"].clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!(
                        "echoed: {}",
                        args["value"]
                    ))],
                    json!({ "value": args["value"] }),
                ))
            }
        },
    ));
    let mut config = base_config();
    config.before_tool_call = Some(Arc::new(|hook, _cancel| {
        Box::pin(async move {
            hook.args["value"] = json!(123);
            None::<BeforeToolCallResult>
        })
    }));

    run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(mock(vec![
            tool_response(
                vec![call("tool-1", "echo", json!({ "value": "hello" }))],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert_eq!(*executed.lock().unwrap(), [json!(123)]);
}

/// Run one blocked tool call and return whether the tool executed plus its tool-result message.
async fn blocked_tool_result(reason: Option<&str>) -> (Arc<AtomicBool>, ToolResultMessage) {
    let executed = Arc::new(AtomicBool::new(false));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                executed.store(true, Ordering::SeqCst);
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!(
                        "echoed: {}",
                        args["value"]
                    ))],
                    json!({}),
                ))
            }
        },
    ));
    let reason = reason.map(str::to_owned);
    let mut config = base_config();
    config.before_tool_call = Some(Arc::new(move |_hook, _cancel| {
        let reason = reason.clone();
        Box::pin(async move {
            Some(BeforeToolCallResult {
                block: true,
                reason,
                terminate: false,
            })
        })
    }));

    let (messages, _events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(mock(vec![
            tool_response(
                vec![call("tool-1", "echo", json!({ "value": "hello" }))],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    let result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("a blocked call still produces a tool-result message");
    (executed, result)
}

// Parity: agent-loop.ts:639 — `beforeResult.reason || "Tool execution was blocked"`. TS
// falsiness means an empty-string reason falls back to the default text.
#[tokio::test]
async fn blocked_call_with_empty_reason_uses_default_text() {
    let (executed, result) = blocked_tool_result(Some("")).await;

    assert!(!executed.load(Ordering::SeqCst));
    assert!(result.is_error);
    assert_eq!(first_text(&result.content), "Tool execution was blocked");
}

// Parity: agent-loop.ts:639 — a non-empty reason is used verbatim.
#[tokio::test]
async fn blocked_call_with_nonempty_reason_uses_the_supplied_reason() {
    let (executed, result) = blocked_tool_result(Some("nope")).await;

    assert!(!executed.load(Ordering::SeqCst));
    assert!(result.is_error);
    assert_eq!(first_text(&result.content), "nope");
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should prepare tool arguments for validation")
#[tokio::test]
async fn prepares_tool_arguments_for_validation() {
    let edit_schema = schema(
        json!({
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": { "type": "string" },
                        "newText": { "type": "string" }
                    },
                    "required": ["oldText", "newText"],
                    "additionalProperties": false
                }
            }
        }),
        &["edits"],
    );
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_by_tool = executed.clone();
    let tool = FnTool::from_value_fn(tool_spec("edit", edit_schema), move |args| {
        let executed = executed_by_tool.clone();
        async move {
            executed.lock().unwrap().push(args["edits"].clone());
            let count = args["edits"].as_array().unwrap().len();
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("edited {count}"))],
                json!({ "count": count }),
            ))
        }
    })
    .with_prepare_arguments(|args| {
        let Some(old_text) = args.get("oldText").and_then(Value::as_str) else {
            return args;
        };
        let Some(new_text) = args.get("newText").and_then(Value::as_str) else {
            return args;
        };
        json!({ "edits": [{ "oldText": old_text, "newText": new_text }] })
    });

    run_new(
        vec![user("edit something")],
        empty_context(vec![Arc::new(tool)]),
        base_config(),
        Some(mock(vec![
            tool_response(
                vec![call(
                    "tool-1",
                    "edit",
                    json!({ "oldText": "before", "newText": "after" }),
                )],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert_eq!(
        *executed.lock().unwrap(),
        [json!([{ "oldText": "before", "newText": "after" }])]
    );
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should emit tool_execution_end in completion order but persist tool results in source order")
#[tokio::test]
async fn tool_execution_end_is_completion_order_results_are_source_order() {
    let first_resolved = Arc::new(AtomicBool::new(false));
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let first_resolved_by_tool = first_resolved.clone();
    let parallel_by_tool = parallel_observed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let first_resolved = first_resolved_by_tool.clone();
            let parallel_observed = parallel_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                if value == "first" {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    first_resolved.store(true, Ordering::SeqCst);
                } else if value == "second" && !first_resolved.load(Ordering::SeqCst) {
                    parallel_observed.store(true, Ordering::SeqCst);
                }
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("echoed: {value}"))],
                    json!({ "value": value }),
                ))
            }
        },
    ));
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (_messages, events) = run_new(
        vec![user("echo both")],
        empty_context(vec![tool]),
        config,
        Some(mock(vec![
            tool_response(
                vec![
                    call("tool-1", "echo", json!({ "value": "first" })),
                    call("tool-2", "echo", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert!(parallel_observed.load(Ordering::SeqCst));
    assert_eq!(tool_end_ids(&events), ["tool-2", "tool-1"]);
    assert_eq!(
        tool_result_ids_from_message_end(&events),
        ["tool-1", "tool-2"]
    );
    assert_eq!(turn_tool_result_ids(&events), ["tool-1", "tool-2"]);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should inject queued messages after all tool calls complete")
#[tokio::test]
async fn injects_queued_messages_after_all_tool_calls_complete() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("ok:{value}"))],
                    json!({ "value": value }),
                ))
            }
        },
    ));
    let queued_delivered = Arc::new(AtomicBool::new(false));
    let queued_for_hook = queued_delivered.clone();
    let executed_for_hook = executed.clone();
    let saw_interrupt = Arc::new(AtomicBool::new(false));
    let saw_interrupt_in_convert = saw_interrupt.clone();
    let convert_calls = Arc::new(AtomicUsize::new(0));
    let convert_calls_in_hook = convert_calls.clone();

    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Sequential;
    config.get_steering_messages = Some(Arc::new(move || {
        let queued = queued_for_hook.clone();
        let executed = executed_for_hook.clone();
        Box::pin(async move {
            if !executed.lock().unwrap().is_empty() && !queued.swap(true, Ordering::SeqCst) {
                vec![user("interrupt")]
            } else {
                Vec::new()
            }
        })
    }));
    config.convert_to_llm = Arc::new(move |messages| {
        let saw_interrupt = saw_interrupt_in_convert.clone();
        let call_index = convert_calls_in_hook.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call_index == 1 {
                saw_interrupt.store(
                    messages
                        .iter()
                        .any(|message| user_text(message) == Some("interrupt")),
                    Ordering::SeqCst,
                );
            }
            rust_genai_agent::convert_messages_to_llm(&messages)
        })
    });

    let (_messages, events) = run_new(
        vec![user("start")],
        empty_context(vec![tool]),
        config,
        Some(mock(vec![
            tool_response(
                vec![
                    call("tool-1", "echo", json!({ "value": "first" })),
                    call("tool-2", "echo", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert_eq!(*executed.lock().unwrap(), ["first", "second"]);
    let tool_ends = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_ends, [false, false]);

    let starts = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart {
                message: AgentMessage::ToolResult(result),
            } => Some(format!("tool:{}", result.tool_call_id)),
            AgentEvent::MessageStart { message } => user_text(message).map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    let interrupt = starts.iter().position(|item| item == "interrupt").unwrap();
    assert!(
        starts
            .iter()
            .position(|item| item == "tool:tool-1")
            .unwrap()
            < interrupt
    );
    assert!(
        starts
            .iter()
            .position(|item| item == "tool:tool-2")
            .unwrap()
            < interrupt
    );
    assert!(saw_interrupt.load(Ordering::SeqCst));
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should force sequential execution when a tool has executionMode=sequential even with default parallel config")
#[tokio::test]
async fn sequential_tool_override_forces_sequential_with_default_parallel_config() {
    let first_resolved = Arc::new(AtomicBool::new(false));
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let first_by_tool = first_resolved.clone();
    let parallel_by_tool = parallel_observed.clone();
    let tool = FnTool::from_value_fn(tool_spec("slow", value_schema()), move |args| {
        let first_resolved = first_by_tool.clone();
        let parallel_observed = parallel_by_tool.clone();
        async move {
            let value = args["value"].as_str().unwrap().to_owned();
            if value == "first" {
                tokio::time::sleep(Duration::from_millis(30)).await;
                first_resolved.store(true, Ordering::SeqCst);
            } else if value == "second" && !first_resolved.load(Ordering::SeqCst) {
                parallel_observed.store(true, Ordering::SeqCst);
            }
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("slow: {value}"))],
                json!({ "value": value }),
            ))
        }
    })
    .with_execution_mode(ToolExecutionMode::Sequential);

    let (_messages, events) = run_new(
        vec![user("run both")],
        empty_context(vec![Arc::new(tool)]),
        base_config(), // Parallel is the global default.
        Some(mock(vec![
            tool_response(
                vec![
                    call("tool-1", "slow", json!({ "value": "first" })),
                    call("tool-2", "slow", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert!(!parallel_observed.load(Ordering::SeqCst));
    assert_eq!(
        tool_result_ids_from_message_end(&events),
        ["tool-1", "tool-2"]
    );
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should force sequential execution when one of multiple tools has executionMode=sequential")
#[tokio::test]
async fn one_sequential_tool_forces_whole_batch_sequential() {
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let slow_finished = Arc::new(AtomicBool::new(false));
    let order_for_slow = order.clone();
    let finished_by_slow = slow_finished.clone();
    let slow = FnTool::from_value_fn(tool_spec("slow", value_schema()), move |args| {
        let order = order_for_slow.clone();
        let finished = finished_by_slow.clone();
        async move {
            let value = args["value"].as_str().unwrap().to_owned();
            order.lock().unwrap().push(format!("slow:{value}"));
            tokio::time::sleep(Duration::from_millis(30)).await;
            finished.store(true, Ordering::SeqCst);
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("slow: {value}"))],
                json!({ "value": value }),
            ))
        }
    })
    .with_execution_mode(ToolExecutionMode::Sequential);

    let order_for_fast = order.clone();
    let finished_for_fast = slow_finished.clone();
    let fast_ran_after_slow = Arc::new(AtomicBool::new(false));
    let fast_after_by_tool = fast_ran_after_slow.clone();
    let fast = FnTool::from_value_fn(tool_spec("fast", value_schema()), move |args| {
        let order = order_for_fast.clone();
        let finished = finished_for_fast.clone();
        let fast_after = fast_after_by_tool.clone();
        async move {
            let value = args["value"].as_str().unwrap().to_owned();
            order.lock().unwrap().push(format!("fast:{value}"));
            fast_after.store(finished.load(Ordering::SeqCst), Ordering::SeqCst);
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("fast: {value}"))],
                json!({ "value": value }),
            ))
        }
    });

    run_new(
        vec![user("run both")],
        empty_context(vec![Arc::new(slow), Arc::new(fast)]),
        base_config(),
        Some(mock(vec![
            tool_response(
                vec![
                    call("tool-1", "slow", json!({ "value": "a" })),
                    call("tool-2", "fast", json!({ "value": "b" })),
                ],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    let order = order.lock().unwrap();
    assert_eq!(order.first().map(String::as_str), Some("slow:a"));
    assert!(order.iter().any(|entry| entry == "fast:b"));
    assert!(fast_ran_after_slow.load(Ordering::SeqCst));
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should allow parallel execution when all tools have executionMode=parallel")
#[tokio::test]
async fn all_parallel_tools_allow_parallel_execution() {
    let first_resolved = Arc::new(AtomicBool::new(false));
    let parallel_observed = Arc::new(AtomicBool::new(false));
    let first_by_tool = first_resolved.clone();
    let parallel_by_tool = parallel_observed.clone();
    let tool = FnTool::from_value_fn(tool_spec("echo", value_schema()), move |args| {
        let first_resolved = first_by_tool.clone();
        let parallel_observed = parallel_by_tool.clone();
        async move {
            let value = args["value"].as_str().unwrap().to_owned();
            if value == "first" {
                tokio::time::sleep(Duration::from_millis(30)).await;
                first_resolved.store(true, Ordering::SeqCst);
            } else if value == "second" && !first_resolved.load(Ordering::SeqCst) {
                parallel_observed.store(true, Ordering::SeqCst);
            }
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("echoed: {value}"))],
                json!({ "value": value }),
            ))
        }
    })
    .with_execution_mode(ToolExecutionMode::Parallel);

    run_new(
        vec![user("echo both")],
        empty_context(vec![Arc::new(tool)]),
        base_config(),
        Some(mock(vec![
            tool_response(
                vec![
                    call("tool-1", "echo", json!({ "value": "first" })),
                    call("tool-2", "echo", json!({ "value": "second" })),
                ],
                StopReason::ToolUse,
            ),
            text_response("done"),
        ])),
    )
    .await;

    assert!(parallel_observed.load(Ordering::SeqCst));
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should use prepareNextTurn snapshot before continuing")
#[tokio::test]
async fn uses_prepare_next_turn_snapshot_before_continuing() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            let value = args["value"].as_str().unwrap().to_owned();
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("echoed: {value}"))],
                json!({ "value": value }),
            ))
        },
    ));
    let prepared = Arc::new(AtomicBool::new(false));
    let prepared_by_hook = prepared.clone();
    let mut config = base_config();
    config.prepare_next_turn = Some(Arc::new(move |hook| {
        let prepared = prepared_by_hook.clone();
        Box::pin(async move {
            if prepared.swap(true, Ordering::SeqCst) {
                None
            } else {
                let mut context = hook.context;
                context.system_prompt = "second prompt".to_owned();
                Some(AgentLoopTurnUpdate {
                    context: Some(context),
                    ..AgentLoopTurnUpdate::default()
                })
            }
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("echo something")],
        AgentContext::new("first prompt").with_tools(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    let calls = stream.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].context.system_prompt, "second prompt");
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should stop after the current turn when shouldStopAfterTurn returns true")
#[tokio::test]
async fn stops_after_current_turn_when_should_stop_after_turn_is_true() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("echoed: {value}"))],
                    json!({ "value": value }),
                ))
            }
        },
    ));
    let steering_polls = Arc::new(AtomicUsize::new(0));
    let steering_by_hook = steering_polls.clone();
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let follow_up_by_hook = follow_up_polls.clone();
    let callback_ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let ids_by_hook = callback_ids.clone();
    let callback_roles = Arc::new(Mutex::new(Vec::<String>::new()));
    let roles_by_hook = callback_roles.clone();

    let mut config = base_config();
    config.get_steering_messages = Some(Arc::new(move || {
        let polls = steering_by_hook.clone();
        Box::pin(async move {
            polls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
    }));
    config.get_follow_up_messages = Some(Arc::new(move || {
        let polls = follow_up_by_hook.clone();
        Box::pin(async move {
            polls.fetch_add(1, Ordering::SeqCst);
            vec![user("follow up should stay queued")]
        })
    }));
    config.should_stop_after_turn = Some(Arc::new(move |hook| {
        let ids = ids_by_hook.clone();
        let roles = roles_by_hook.clone();
        Box::pin(async move {
            assert_eq!(hook.message.stop_reason, StopReason::ToolUse);
            *ids.lock().unwrap() = hook
                .tool_results
                .iter()
                .map(|result| result.tool_call_id.clone())
                .collect();
            *roles.lock().unwrap() = hook
                .context
                .messages
                .iter()
                .map(|message| message.role().to_owned())
                .collect();
            true
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("should not run"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert_eq!(stream.call_count(), 1);
    assert_eq!(*executed.lock().unwrap(), ["hello"]);
    assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
    assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
    assert_eq!(*callback_ids.lock().unwrap(), ["tool-1"]);
    assert_eq!(
        *callback_roles.lock().unwrap(),
        ["user", "assistant", "tool_result"]
    );
    assert_eq!(roles(&messages), ["user", "assistant", "tool_result"]);
    assert_eq!(
        event_kinds(&events),
        [
            EventKind::AgentStart,
            EventKind::TurnStart,
            EventKind::MessageStart,
            EventKind::MessageEnd,
            EventKind::MessageStart,
            EventKind::MessageEnd,
            EventKind::ToolExecutionStart,
            EventKind::ToolExecutionEnd,
            EventKind::MessageStart,
            EventKind::MessageEnd,
            EventKind::TurnEnd,
            EventKind::AgentEnd,
        ]
    );
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should stop after a tool batch when every tool result sets terminate=true")
#[tokio::test]
async fn stops_after_tool_batch_when_every_result_terminates() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            let value = args["value"].as_str().unwrap().to_owned();
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("echoed: {value}"))],
                json!({ "value": value }),
            )
            .with_terminate(true))
        },
    ));
    let stream = mock(vec![tool_response(
        vec![call("tool-1", "echo", json!({ "value": "hello" }))],
        StopReason::ToolUse,
    )]);

    let (messages, events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        base_config(),
        Some(stream.clone()),
    )
    .await;

    assert_eq!(stream.call_count(), 1);
    assert_eq!(roles(&messages), ["user", "assistant", "tool_result"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should stop after a blocked tool call when beforeToolCall sets terminate=true")
#[tokio::test]
async fn stops_after_blocked_call_when_before_hook_sets_terminate() {
    let executed = Arc::new(AtomicBool::new(false));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |_args| {
            let executed = executed_by_tool.clone();
            async move {
                executed.store(true, Ordering::SeqCst);
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text("should not execute")],
                    json!({ "value": "unexpected" }),
                ))
            }
        },
    ));
    let mut config = base_config();
    config.before_tool_call = Some(Arc::new(|_hook, _cancel| {
        Box::pin(async {
            Some(BeforeToolCallResult {
                block: true,
                reason: Some("Blocked by policy".to_owned()),
                terminate: true,
            })
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("should not run"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    let result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("the blocked call still produces a tool-result message");
    assert!(!executed.load(Ordering::SeqCst));
    assert_eq!(stream.call_count(), 1);
    assert!(result.is_error);
    assert_eq!(first_text(&result.content), "Blocked by policy");
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should continue after a mixed batch with one terminating blocked call")
#[tokio::test]
async fn continues_after_mixed_batch_with_one_terminating_blocked_call() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::new(
                    vec![ToolResultContent::text(format!("echoed: {value}"))],
                    json!({ "value": value }),
                ))
            }
        },
    ));
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;
    config.before_tool_call = Some(Arc::new(|hook, _cancel| {
        let is_first = hook.args["value"] == json!("first");
        Box::pin(async move {
            is_first.then(|| BeforeToolCallResult {
                block: true,
                reason: Some("Blocked first".to_owned()),
                terminate: true,
            })
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![
                call("tool-1", "echo", json!({ "value": "first" })),
                call("tool-2", "echo", json!({ "value": "second" })),
            ],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("echo both")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert_eq!(*executed.lock().unwrap(), ["second"]);
    assert_eq!(stream.call_count(), 2);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should continue after parallel tool calls when not all tool results terminate")
#[tokio::test]
async fn continues_after_parallel_calls_when_not_all_results_terminate() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            let value = args["value"].as_str().unwrap().to_owned();
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("echoed: {value}"))],
                json!({ "value": value }),
            )
            .with_terminate(value == "first"))
        },
    ));
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;
    let stream = mock(vec![
        tool_response(
            vec![
                call("tool-1", "echo", json!({ "value": "first" })),
                call("tool-2", "echo", json!({ "value": "second" })),
            ],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo both")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert_eq!(stream.call_count(), 2);
    assert_eq!(
        roles(&messages),
        [
            "user",
            "assistant",
            "tool_result",
            "tool_result",
            "assistant"
        ]
    );
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should allow afterToolCall to mark a tool batch as terminating")
#[tokio::test]
async fn after_tool_call_can_mark_batch_as_terminating() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            let value = args["value"].as_str().unwrap().to_owned();
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("echoed: {value}"))],
                json!({ "value": value }),
            ))
        },
    ));
    let mut config = base_config();
    config.after_tool_call = Some(Arc::new(|_hook, _cancel| {
        Box::pin(async {
            Some(AfterToolCallResult {
                terminate: Some(true),
                ..AfterToolCallResult::default()
            })
        })
    }));
    let stream = mock(vec![tool_response(
        vec![call("tool-1", "echo", json!({ "value": "hello" }))],
        StopReason::ToolUse,
    )]);

    run_new(
        vec![user("echo something")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert_eq!(stream.call_count(), 1);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should throw when context has no messages")
#[tokio::test]
async fn continue_errors_when_context_has_no_messages() {
    let error = run_continue(
        AgentContext::new("You are helpful."),
        base_config(),
        Some(mock(Vec::new())),
    )
    .await
    .expect_err("an empty continuation context must be rejected");

    assert!(matches!(error, LoopError::EmptyContext));
    assert_eq!(error.to_string(), "Cannot continue: no messages in context");
}

// Parity: pi agent stream-fn.ts:17 — the Rust-adapted spelling of the missing-default error text.
// Both error enums must keep the identical string.
#[test]
fn no_default_stream_fn_error_text_is_pinned() {
    let expected = "No default stream function configured. Pass stream_fn explicitly or call set_default_stream_fn().";
    assert_eq!(LoopError::NoDefaultStreamFn.to_string(), expected);
    assert_eq!(AgentError::NoDefaultStreamFn.to_string(), expected);
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should continue from existing context without emitting user message events")
#[tokio::test]
async fn continues_existing_context_without_user_message_events() {
    let context = AgentContext::new("You are helpful.").with_messages(vec![user("Hello")]);
    let (messages, events) = run_continue(
        context,
        base_config(),
        Some(mock(vec![text_response("Response")])),
    )
    .await
    .expect("valid continuation");

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role(), "assistant");
    let message_ends = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(message_ends.len(), 1);
    assert_eq!(message_ends[0].role(), "assistant");
}

// TS: pi/packages/agent/test/agent-loop.test.ts
// it("should allow custom message types as last message (caller responsibility)")
#[tokio::test]
async fn allows_custom_message_as_last_message_for_continuation() {
    let context = AgentContext::new("You are helpful.").with_messages(vec![AgentMessage::Custom(
        CustomMessage::new("custom", json!({ "text": "Hook content" })),
    )]);
    let mut config = base_config();
    config.convert_to_llm = Arc::new(|messages| {
        Box::pin(async move {
            let mapped = messages
                .into_iter()
                .map(|message| match message {
                    AgentMessage::Custom(custom) if custom.role == "custom" => AgentMessage::user(
                        custom.data["text"].as_str().unwrap_or_default().to_owned(),
                    ),
                    other => other,
                })
                .collect::<Vec<_>>();
            rust_genai_agent::convert_messages_to_llm(&mapped)
        })
    });
    let stream = mock(vec![text_response("Response to custom message")]);

    let (messages, _events) = run_continue(context, config, Some(stream.clone()))
        .await
        .expect("custom last messages are the converter's responsibility");

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role(), "assistant");
    assert_eq!(stream.calls()[0].context.messages[0].role, ChatRole::User);
}

// ---------------------------------------------------------------------------
// CORE-REQ-01: independent session/retry request fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop_forwards_session_id_and_retry_fields_onto_stream_requests() {
    let config = base_config()
        .with_session_id("loop-session")
        .with_max_retries(4)
        .with_max_retry_delay_ms(7_500)
        .with_chat_options(ChatOptions::default().with_prompt_cache_key("explicit-key"));
    let stream = mock(vec![text_response("one"), text_response("two")]);

    run_new(
        vec![user("hi")],
        AgentContext::default(),
        config.clone(),
        Some(stream.clone()),
    )
    .await;
    run_continue(
        AgentContext::new("").with_messages(vec![user("again")]),
        config,
        Some(stream.clone()),
    )
    .await
    .expect("valid continuation");

    // Both the prompt and the continue path carry the same immutable per-run snapshot fields, and
    // the session id never writes the explicit cache key (nor vice versa).
    let calls = stream.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.session_id.as_deref(), Some("loop-session"));
        assert_eq!(call.max_retries, Some(4));
        assert_eq!(call.max_retry_delay_ms, Some(7_500));
        assert_eq!(
            call.options.prompt_cache_key.as_deref(),
            Some("explicit-key")
        );
    }
}

#[tokio::test]
async fn stream_request_session_and_retry_fields_default_to_none() {
    let stream = mock(vec![text_response("ok")]);

    run_new(
        vec![user("hi")],
        AgentContext::default(),
        base_config(),
        Some(stream.clone()),
    )
    .await;

    let calls = stream.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].session_id, None);
    assert_eq!(calls[0].max_retries, None);
    assert_eq!(calls[0].max_retry_delay_ms, None);
    assert_eq!(calls[0].options.prompt_cache_key, None);
}

// ---------------------------------------------------------------------------
// CORE-THINK-01: next-turn thinking updates reuse the ThinkingBudgets map
// ---------------------------------------------------------------------------

#[tokio::test]
async fn next_turn_thinking_updates_resolve_through_thinking_budgets() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            Ok(AgentToolResult::text(format!(
                "echoed: {}",
                args["value"].as_str().unwrap()
            )))
        },
    ));
    // The low-level loop receives the caller's chat options untouched for the initial request.
    let updates = [
        ThinkingLevel::Low,        // resolves through the custom `low` entry
        ThinkingLevel::XHigh,      // clamps through the custom `high` entry
        ThinkingLevel::Budget(64), // explicit budget bypasses the map
        ThinkingLevel::Off,        // clears the reasoning effort
    ];
    let turn = Arc::new(AtomicUsize::new(0));
    let turn_by_hook = turn.clone();
    let mut config = base_config();
    config = config
        .with_thinking_budgets(ThinkingBudgets::default().with_low(512).with_high(900))
        .with_chat_options(ChatOptions::default().with_reasoning_effort(ReasoningEffort::Medium));
    config.prepare_next_turn = Some(Arc::new(move |_hook| {
        let update = updates
            .get(turn_by_hook.fetch_add(1, Ordering::SeqCst))
            .copied();
        Box::pin(async move {
            update.map(|thinking_level| AgentLoopTurnUpdate {
                thinking_level: Some(thinking_level),
                ..AgentLoopTurnUpdate::default()
            })
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "a" }))],
            StopReason::ToolUse,
        ),
        tool_response(
            vec![call("tool-2", "echo", json!({ "value": "b" }))],
            StopReason::ToolUse,
        ),
        tool_response(
            vec![call("tool-3", "echo", json!({ "value": "c" }))],
            StopReason::ToolUse,
        ),
        tool_response(
            vec![call("tool-4", "echo", json!({ "value": "d" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("go")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    let calls = stream.calls();
    assert_eq!(calls.len(), 5);
    assert!(matches!(
        calls[0].options.reasoning_effort,
        Some(ReasoningEffort::Medium)
    ));
    assert!(
        matches!(
            calls[1].options.reasoning_effort,
            Some(ReasoningEffort::Budget(512))
        ),
        "Low resolves through the custom `low` budget entry"
    );
    assert!(
        matches!(
            calls[2].options.reasoning_effort,
            Some(ReasoningEffort::Budget(900))
        ),
        "XHigh clamps through the custom `high` budget entry"
    );
    assert!(
        matches!(
            calls[3].options.reasoning_effort,
            Some(ReasoningEffort::Budget(64))
        ),
        "an explicit budget bypasses the map"
    );
    assert!(
        calls[4].options.reasoning_effort.is_none(),
        "Off clears the reasoning effort for the next request"
    );
}

#[tokio::test]
async fn next_turn_named_level_without_budget_entry_falls_back_to_named_effort() {
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        |args| async move {
            Ok(AgentToolResult::text(format!(
                "echoed: {}",
                args["value"].as_str().unwrap()
            )))
        },
    ));
    // Only `low` is configured: a High update falls back to the named reasoning effort.
    let updates = [ThinkingLevel::High];
    let turn = Arc::new(AtomicUsize::new(0));
    let turn_by_hook = turn.clone();
    let mut config = base_config().with_thinking_budgets(ThinkingBudgets::default().with_low(512));
    config.prepare_next_turn = Some(Arc::new(move |_hook| {
        let update = updates
            .get(turn_by_hook.fetch_add(1, Ordering::SeqCst))
            .copied();
        Box::pin(async move {
            update.map(|thinking_level| AgentLoopTurnUpdate {
                thinking_level: Some(thinking_level),
                ..AgentLoopTurnUpdate::default()
            })
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "a" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("go")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    let calls = stream.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        matches!(
            calls[1].options.reasoning_effort,
            Some(ReasoningEffort::High)
        ),
        "an unconfigured level keeps its named reasoning effort"
    );
}

// ---------------------------------------------------------------------------
// CORE-TOOLERR-01: fallible preparation and before/after hook channels
// ---------------------------------------------------------------------------

fn recording_echo_tool(executed: Arc<Mutex<Vec<Value>>>) -> Arc<FnTool> {
    let executed_by_tool = executed.clone();
    Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                executed.lock().unwrap().push(args["value"].clone());
                Ok(AgentToolResult::text(format!(
                    "echoed: {}",
                    args["value"].as_str().unwrap()
                )))
            }
        },
    ))
}

fn tool_result_texts(messages: &[AgentMessage]) -> Vec<(bool, String)> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => {
                Some((result.is_error, first_text(&result.content).to_owned()))
            }
            _ => None,
        })
        .collect()
}

fn tool_execution_end_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn try_prepare_arguments_error_skips_execution_and_continues_sequential() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = FnTool::from_value_fn(tool_spec("echo", value_schema()), {
        let executed = executed.clone();
        move |args| {
            let executed = executed.clone();
            async move {
                executed.lock().unwrap().push(args["value"].clone());
                Ok(AgentToolResult::text("should not execute"))
            }
        }
    })
    .with_try_prepare_arguments(|_args| Err(ToolHookError::new("cannot prepare these arguments")));
    let config = base_config().with_tool_execution(ToolExecutionMode::Sequential);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("recovered"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo something")],
        empty_context(vec![Arc::new(tool)]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert!(executed.lock().unwrap().is_empty(), "execution skipped");
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "cannot prepare these arguments".to_owned())]
    );
    assert_eq!(
        tool_execution_end_ids(&events),
        ["tool-1"],
        "the immediate failure still emits start/end for the call"
    );
    assert_eq!(
        stream.call_count(),
        2,
        "a preparation failure is an ordinary error result; the loop continues"
    );
    assert_eq!(messages.last().unwrap().role(), "assistant");
}

#[tokio::test]
async fn try_prepare_arguments_takes_precedence_over_legacy_transform() {
    let legacy_calls = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = FnTool::from_value_fn(tool_spec("echo", value_schema()), {
        let executed = executed.clone();
        move |args| {
            let executed = executed.clone();
            async move {
                executed.lock().unwrap().push(args["value"].clone());
                Ok(AgentToolResult::text("ok"))
            }
        }
    })
    .with_prepare_arguments({
        let legacy_calls = legacy_calls.clone();
        move |mut args| {
            legacy_calls.fetch_add(1, Ordering::SeqCst);
            args["value"] = json!("legacy");
            args
        }
    })
    .with_try_prepare_arguments(|mut args| {
        args["value"] = json!("fallible");
        Ok(args)
    });
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "raw" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("echo")],
        empty_context(vec![Arc::new(tool)]),
        base_config(),
        Some(stream),
    )
    .await;

    assert_eq!(
        legacy_calls.load(Ordering::SeqCst),
        0,
        "the legacy transform is never invoked when the fallible channel is installed"
    );
    assert_eq!(*executed.lock().unwrap(), [json!("fallible")]);
}

/// Custom tool overriding only the legacy infallible transform: the default fallible entry point
/// must adapt it unchanged.
struct LegacyOnlyPrepareTool {
    executed: Mutex<Vec<Value>>,
}

#[async_trait]
impl AgentTool for LegacyOnlyPrepareTool {
    fn spec(&self) -> ToolSpec {
        tool_spec("legacy", value_schema())
    }

    fn prepare_arguments(&self, mut args: Value) -> Value {
        args["value"] = json!("prepared-by-legacy");
        args
    }

    async fn execute(
        &self,
        call: ToolCallContext,
        _cancel: CancellationToken,
        _on_update: UpdateSink,
    ) -> Result<AgentToolResult, ToolError> {
        self.executed
            .lock()
            .unwrap()
            .push(call.args["value"].clone());
        Ok(AgentToolResult::text(format!(
            "got {}",
            call.args["value"].as_str().unwrap()
        )))
    }
}

#[tokio::test]
async fn legacy_prepare_arguments_is_adapted_into_the_fallible_channel() {
    let tool = Arc::new(LegacyOnlyPrepareTool {
        executed: Mutex::new(Vec::new()),
    });
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "legacy", json!({ "value": "raw" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    let (messages, _events) = run_new(
        vec![user("go")],
        empty_context(vec![tool.clone()]),
        base_config(),
        Some(stream),
    )
    .await;

    assert_eq!(
        *tool.executed.lock().unwrap(),
        [json!("prepared-by-legacy")]
    );
    assert_eq!(
        tool_result_texts(&messages),
        [(false, "got prepared-by-legacy".to_owned())]
    );
}

/// Custom tool overriding both channels: only the fallible entry point runs.
struct FalliblePrepareTool {
    legacy_calls: AtomicUsize,
}

#[async_trait]
impl AgentTool for FalliblePrepareTool {
    fn spec(&self) -> ToolSpec {
        tool_spec("fallible", value_schema())
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        args
    }

    fn try_prepare_arguments(&self, _args: Value) -> Result<Value, ToolHookError> {
        Err(ToolHookError::new("preparation exploded"))
    }

    async fn execute(
        &self,
        _call: ToolCallContext,
        _cancel: CancellationToken,
        _on_update: UpdateSink,
    ) -> Result<AgentToolResult, ToolError> {
        panic!("execution must be skipped after a preparation failure")
    }
}

#[tokio::test]
async fn custom_tool_try_prepare_arguments_failure_becomes_in_band_error() {
    let tool = Arc::new(FalliblePrepareTool {
        legacy_calls: AtomicUsize::new(0),
    });
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "fallible", json!({ "value": "x" }))],
            StopReason::ToolUse,
        ),
        text_response("recovered"),
    ]);

    let (messages, _events) = run_new(
        vec![user("go")],
        empty_context(vec![tool.clone()]),
        base_config(),
        Some(stream.clone()),
    )
    .await;

    assert_eq!(tool.legacy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "preparation exploded".to_owned())]
    );
    assert_eq!(stream.call_count(), 2, "the loop continues after the error");
}

#[tokio::test]
async fn try_before_tool_call_error_skips_execution_sequential() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = recording_echo_tool(executed.clone());
    let try_before: TryBeforeToolCallHook = Arc::new(|_context, _cancel| {
        Box::pin(async { Err(ToolHookError::new("before hook exploded")) })
    });
    let config = base_config()
        .with_tool_execution(ToolExecutionMode::Sequential)
        .with_try_before_tool_call(try_before);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("recovered"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert!(executed.lock().unwrap().is_empty(), "execution skipped");
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "before hook exploded".to_owned())]
    );
    assert_eq!(tool_execution_end_ids(&events), ["tool-1"]);
    assert_eq!(
        stream.call_count(),
        2,
        "a before-hook failure does not request termination; the loop continues"
    );
}

#[tokio::test]
async fn try_before_tool_call_takes_precedence_over_legacy_hook() {
    let legacy_calls = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = recording_echo_tool(executed.clone());
    let mut config = base_config();
    config.before_tool_call = Some({
        let legacy_calls = legacy_calls.clone();
        Arc::new(move |_context, _cancel| {
            legacy_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { None::<BeforeToolCallResult> })
        })
    });
    config.try_before_tool_call = Some(Arc::new(|_context, _cancel| {
        Box::pin(async {
            Ok(Some(BeforeToolCallResult {
                block: true,
                reason: Some("blocked by the fallible hook".to_owned()),
                terminate: false,
            }))
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert_eq!(
        legacy_calls.load(Ordering::SeqCst),
        0,
        "the legacy hook is never invoked when the fallible hook is installed"
    );
    assert!(executed.lock().unwrap().is_empty());
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "blocked by the fallible hook".to_owned())]
    );
}

#[tokio::test]
async fn try_before_tool_call_ok_block_with_terminate_stops_the_batch() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = recording_echo_tool(executed.clone());
    let try_before: TryBeforeToolCallHook = Arc::new(|_context, _cancel| {
        Box::pin(async {
            Ok(Some(BeforeToolCallResult {
                block: true,
                reason: Some("terminating block".to_owned()),
                terminate: true,
            }))
        })
    });
    let config = base_config().with_try_before_tool_call(try_before);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("should not run"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert!(executed.lock().unwrap().is_empty());
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "terminating block".to_owned())]
    );
    assert_eq!(
        stream.call_count(),
        1,
        "a terminating blocked call ends the loop like the legacy channel"
    );
}

#[tokio::test]
async fn try_before_tool_call_ok_none_applies_arg_mutations_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executed_by_tool = executed.clone();
    let tool = Arc::new(FnTool::from_value_fn(
        tool_spec("echo", value_schema()),
        move |args| {
            let executed = executed_by_tool.clone();
            async move {
                executed.lock().unwrap().push(args["value"].clone());
                Ok(AgentToolResult::text(format!("echoed: {}", args["value"])))
            }
        },
    ));
    let try_before: TryBeforeToolCallHook = Arc::new(|context, _cancel| {
        context.args["value"] = json!(123);
        Box::pin(async { Ok(None::<BeforeToolCallResult>) })
    });
    let config = base_config().with_try_before_tool_call(try_before);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream),
    )
    .await;

    assert_eq!(*executed.lock().unwrap(), [json!(123)]);
}

#[tokio::test]
async fn try_before_tool_call_cancellation_yields_operation_aborted() {
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = recording_echo_tool(executed.clone());
    let try_before: TryBeforeToolCallHook = Arc::new(|_context, hook_cancel| {
        Box::pin(async move {
            // The hook receives the live run token and cancels the run itself.
            assert!(!hook_cancel.is_cancelled());
            hook_cancel.cancel();
            Ok(None::<BeforeToolCallResult>)
        })
    });
    let config = base_config().with_try_before_tool_call(try_before);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("after abort"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert!(executed.lock().unwrap().is_empty(), "execution skipped");
    assert_eq!(
        tool_result_texts(&messages),
        [(true, "Operation aborted".to_owned())]
    );
    // A cancellation observed in the before channel produces an ordinary (non-terminating) error
    // result, so the batch ends but the loop still continues to the next turn.
    assert_eq!(stream.call_count(), 2);
}

#[tokio::test]
async fn try_after_tool_call_error_replaces_completed_result_sequential() {
    let executed = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(FnTool::from_value_fn(tool_spec("echo", value_schema()), {
        let executed = executed.clone();
        move |args| {
            let executed = executed.clone();
            async move {
                executed.store(true, Ordering::SeqCst);
                Ok(
                    AgentToolResult::text(format!("echoed: {}", args["value"].as_str().unwrap()))
                        .with_usage(AgentUsage::new(1, 2))
                        .with_terminate(true),
                )
            }
        }
    }));
    let try_after: TryAfterToolCallHook = Arc::new(|_context, _cancel| {
        Box::pin(async { Err(ToolHookError::new("after hook exploded")) })
    });
    let config = base_config()
        .with_tool_execution(ToolExecutionMode::Sequential)
        .with_try_after_tool_call(try_after);
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("recovered"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream.clone()),
    )
    .await;

    assert!(
        executed.load(Ordering::SeqCst),
        "tool side effects are not rolled back by an after-hook failure"
    );
    let result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("tool result message");
    assert!(result.is_error);
    assert_eq!(first_text(&result.content), "after hook exploded");
    assert_eq!(
        result.details,
        json!({}),
        "the replacement discards details"
    );
    assert_eq!(result.usage, None, "the replacement discards usage");
    assert_eq!(tool_execution_end_ids(&events), ["tool-1"]);
    assert_eq!(
        stream.call_count(),
        2,
        "the replacement also discards the tool's termination request, so the loop continues"
    );
}

#[tokio::test]
async fn try_after_tool_call_takes_precedence_over_legacy_hook() {
    let legacy_calls = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let tool = recording_echo_tool(executed.clone());
    let mut config = base_config();
    config.after_tool_call = Some({
        let legacy_calls = legacy_calls.clone();
        Arc::new(move |_context, _cancel| {
            legacy_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { None::<AfterToolCallResult> })
        })
    });
    config.try_after_tool_call = Some(Arc::new(|_context, _cancel| {
        Box::pin(async {
            Ok(Some(AfterToolCallResult {
                details: Some(json!({ "channel": "fallible" })),
                ..AfterToolCallResult::default()
            }))
        })
    }));
    let stream = mock(vec![
        tool_response(
            vec![call("tool-1", "echo", json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]);

    let (messages, _events) = run_new(
        vec![user("echo")],
        empty_context(vec![tool]),
        config,
        Some(stream),
    )
    .await;

    assert_eq!(
        legacy_calls.load(Ordering::SeqCst),
        0,
        "the legacy hook is never invoked when the fallible hook is installed"
    );
    assert_eq!(*executed.lock().unwrap(), [json!("hello")]);
    let result = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("tool result message");
    assert!(!result.is_error);
    assert_eq!(result.details, json!({ "channel": "fallible" }));
}

#[tokio::test]
async fn parallel_fallible_channel_failures_keep_source_order_and_continue() {
    let executed = Arc::new(Mutex::new(Vec::<String>::new()));
    let tool = FnTool::from_value_fn(tool_spec("echo", value_schema()), {
        let executed = executed.clone();
        move |args| {
            let executed = executed.clone();
            async move {
                let value = args["value"].as_str().unwrap().to_owned();
                executed.lock().unwrap().push(value.clone());
                Ok(AgentToolResult::text(format!("echoed: {value}")))
            }
        }
    })
    .with_try_prepare_arguments(|args| {
        if args["value"] == json!("bad-prepare") {
            Err(ToolHookError::new("prepare failed in parallel"))
        } else {
            Ok(args)
        }
    });
    let try_before: TryBeforeToolCallHook = Arc::new(|context, _cancel| {
        let failed = context.args["value"] == json!("bad-before");
        Box::pin(async move {
            if failed {
                Err(ToolHookError::new("before failed in parallel"))
            } else {
                Ok(None::<BeforeToolCallResult>)
            }
        })
    });
    let try_after: TryAfterToolCallHook = Arc::new(|context, _cancel| {
        let failed = context.args["value"] == json!("bad-after");
        Box::pin(async move {
            if failed {
                Err(ToolHookError::new("after failed in parallel"))
            } else {
                Ok(None::<AfterToolCallResult>)
            }
        })
    });
    let config = base_config()
        .with_tool_execution(ToolExecutionMode::Parallel)
        .with_try_before_tool_call(try_before)
        .with_try_after_tool_call(try_after);
    let stream = mock(vec![
        tool_response(
            vec![
                call("tool-1", "echo", json!({ "value": "good" })),
                call("tool-2", "echo", json!({ "value": "bad-prepare" })),
                call("tool-3", "echo", json!({ "value": "bad-before" })),
                call("tool-4", "echo", json!({ "value": "bad-after" })),
            ],
            StopReason::ToolUse,
        ),
        text_response("recovered"),
    ]);

    let (messages, events) = run_new(
        vec![user("echo all")],
        empty_context(vec![Arc::new(tool)]),
        config,
        Some(stream.clone()),
    )
    .await;

    // Only the good and bad-after calls execute; preparation/before failures skip execution.
    let mut executed_values = executed.lock().unwrap().clone();
    executed_values.sort();
    assert_eq!(executed_values, ["bad-after", "good"]);

    // Start events are source ordered; the immediate preflight failures end before the executed
    // calls, and every call still emits exactly one end event.
    let start_ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(start_ids, ["tool-1", "tool-2", "tool-3", "tool-4"]);
    let end_ids = tool_execution_end_ids(&events);
    assert_eq!(end_ids.len(), 4);
    assert_eq!(
        &end_ids[..2],
        ["tool-2", "tool-3"],
        "immediate preparation/before failures end during source-ordered preflight"
    );
    let mut executed_end_ids = end_ids[2..].to_vec();
    executed_end_ids.sort();
    assert_eq!(executed_end_ids, ["tool-1", "tool-4"]);

    // Result messages are assembled in source order with per-call error classification and the
    // exact in-band texts.
    let results: Vec<(String, bool, String)> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => Some((
                result.tool_call_id.clone(),
                result.is_error,
                first_text(&result.content).to_owned(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            ("tool-1".to_owned(), false, "echoed: good".to_owned()),
            (
                "tool-2".to_owned(),
                true,
                "prepare failed in parallel".to_owned()
            ),
            (
                "tool-3".to_owned(),
                true,
                "before failed in parallel".to_owned()
            ),
            (
                "tool-4".to_owned(),
                true,
                "after failed in parallel".to_owned()
            ),
        ]
    );
    assert_eq!(
        stream.call_count(),
        2,
        "the loop continues after a mixed batch of preparation and hook failures"
    );
    assert_eq!(messages.last().unwrap().role(), "assistant");
}
