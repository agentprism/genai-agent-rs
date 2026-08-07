use genai::adapter::AdapterKind;
use genai::chat::{
    ChatStreamEvent, ContentPart, MessageContent, StopReason as GenaiStopReason, StreamChunk,
    StreamEnd, ToolCall, ToolChunk, Usage,
};
use genai::{Client, ModelIden};
use rust_genai_agent::{
    AgentToolCall, AssistantAccumulator, AssistantContent, AssistantMessageEvent, GenaiStreamFn,
    LlmContext, StopReason, StreamFn, StreamRequest, parse_streaming_json,
};
use serde_json::{Value, json};

fn model() -> ModelIden {
    ModelIden::new(AdapterKind::OpenAI, "test-model")
}

fn text_chunk(text: &str) -> ChatStreamEvent {
    ChatStreamEvent::Chunk(StreamChunk {
        content: text.to_string(),
    })
}

fn reasoning_chunk(text: &str) -> ChatStreamEvent {
    ChatStreamEvent::ReasoningChunk(StreamChunk {
        content: text.to_string(),
    })
}

fn thought_chunk(text: &str) -> ChatStreamEvent {
    ChatStreamEvent::ThoughtSignatureChunk(StreamChunk {
        content: text.to_string(),
    })
}

fn tool_chunk(id: &str, name: &str, arguments: Value) -> ChatStreamEvent {
    ChatStreamEvent::ToolCallChunk(ToolChunk {
        tool_call: ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: arguments,
            thought_signatures: None,
        },
    })
}

fn event_name(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "tool_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "tool_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "tool_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

#[test]
fn folds_reasoning_text_heartbeat_and_final_metadata() {
    let mut accumulator = AssistantAccumulator::new(model());
    let mut events = Vec::new();
    for event in [
        ChatStreamEvent::Start,
        reasoning_chunk("careful plan"),
        thought_chunk("sig-"),
        thought_chunk("tail"),
        text_chunk("answer"),
        ChatStreamEvent::Heartbeat,
    ] {
        events.extend(accumulator.fold(event));
    }
    events.extend(accumulator.fold(ChatStreamEvent::End(StreamEnd {
        captured_usage: Some(Usage {
            prompt_tokens: Some(11),
            completion_tokens: Some(7),
            total_tokens: Some(18),
            ..Default::default()
        }),
        captured_stop_reason: Some(GenaiStopReason::Completed("end_turn".to_string())),
        captured_content: Some(MessageContent::from_text("answer")),
        captured_reasoning_content: Some("careful plan".to_string()),
        captured_response_id: Some("resp_123".to_string()),
    })));

    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "text_start",
            "text_delta",
            "thinking_end",
            "text_end",
            "done",
        ]
    );

    let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
        panic!("expected done event");
    };
    assert_eq!(*reason, StopReason::Stop);
    assert_eq!(message.provider_stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(message.response_id.as_deref(), Some("resp_123"));
    assert_eq!(message.usage.input_tokens, 11);
    assert_eq!(message.usage.output_tokens, 7);
    assert_eq!(message.usage.total_tokens, 18);
    assert_eq!(message.model.model_name, "test-model");
    assert!(matches!(
        &message.content[0],
        AssistantContent::Thinking { thinking, signature }
            if thinking == "careful plan" && signature.as_deref() == Some("sig-tail")
    ));
    assert!(matches!(
        &message.content[1],
        AssistantContent::Text { text, .. } if text == "answer"
    ));
}

#[test]
fn cumulative_tool_snapshots_emit_only_raw_suffixes_and_salvage_partial_json() {
    let mut accumulator = AssistantAccumulator::new(model());
    let mut events = accumulator.fold(ChatStreamEvent::Start);
    events.extend(accumulator.fold(tool_chunk(
        "call_1",
        "weather",
        Value::String(String::new()),
    )));
    events.extend(accumulator.fold(tool_chunk(
        "call_1",
        "weather",
        Value::String(r#"{"city":"San"#.to_string()),
    )));
    events.extend(accumulator.fold(tool_chunk(
        "call_1",
        "weather",
        Value::String(r#"{"city":"San Francisco","meta":{"unit":"C"#.to_string()),
    )));

    let deltas = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta { delta, partial, .. } => {
                Some((delta.clone(), partial.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 2);
    assert_eq!(deltas[0].0, r#"{"city":"San"#);
    assert_eq!(deltas[1].0, r#" Francisco","meta":{"unit":"C"#);
    let partial_call = deltas[1].1.tool_calls().next().unwrap();
    assert_eq!(partial_call.arguments["city"], "San Francisco");
    assert_eq!(partial_call.arguments["meta"]["unit"], "C");

    events.extend(accumulator.fold(ChatStreamEvent::End(StreamEnd {
        captured_stop_reason: Some(GenaiStopReason::ToolCall("tool_calls".to_string())),
        captured_content: Some(MessageContent::from_tool_calls(vec![ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "weather".to_string(),
            fn_arguments: json!({"city": "San Francisco", "meta": {"unit": "C"}}),
            thought_signatures: None,
        }])),
        ..Default::default()
    })));

    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        [
            "start",
            "tool_start",
            "tool_delta",
            "tool_delta",
            "tool_end",
            "done",
        ]
    );
    let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = &events[4] else {
        panic!("expected final tool call");
    };
    assert_eq!(
        tool_call.arguments,
        json!({"city": "San Francisco", "meta": {"unit": "C"}})
    );
    let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
        panic!("expected done");
    };
    assert_eq!(*reason, StopReason::ToolUse);
    assert_eq!(message.provider_stop_reason.as_deref(), Some("tool_calls"));
}

#[test]
fn end_can_materialize_a_captured_only_tool_call_and_preserve_thoughts() {
    let mut accumulator = AssistantAccumulator::new(model());
    let call = ToolCall {
        call_id: "call_final".to_string(),
        fn_name: "lookup".to_string(),
        fn_arguments: json!({"query": "rust"}),
        thought_signatures: Some(vec!["opaque-signature".to_string()]),
    };
    let events = accumulator.fold(ChatStreamEvent::End(StreamEnd {
        captured_content: Some(MessageContent::from_parts(vec![
            ContentPart::ThoughtSignature("opaque-signature".to_string()),
            ContentPart::ToolCall(call),
        ])),
        captured_response_id: Some("response-final".to_string()),
        ..Default::default()
    }));

    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        ["start", "tool_start", "tool_delta", "tool_end", "done"]
    );
    let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
        panic!("expected done");
    };
    assert_eq!(*reason, StopReason::ToolUse);
    assert_eq!(message.response_id.as_deref(), Some("response-final"));
    let call = message.tool_calls().next().unwrap();
    assert_eq!(call.arguments, json!({"query": "rust"}));
    assert_eq!(call.thought_signatures, ["opaque-signature"]);
}

#[test]
fn stream_errors_and_cancellation_are_terminal_in_band_and_keep_partial_content() {
    let mut accumulator = AssistantAccumulator::new(model());
    accumulator.fold(text_chunk("partial"));
    let events = accumulator.fold_result::<std::io::Error>(Err(std::io::Error::other("boom")));
    assert_eq!(events.iter().map(event_name).collect::<Vec<_>>(), ["error"]);
    let AssistantMessageEvent::Error { reason, error } = &events[0] else {
        panic!("expected error");
    };
    assert_eq!(*reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("boom"));
    assert_eq!(error.text(), "partial");
    assert!(accumulator.fold(text_chunk("ignored")).is_empty());

    let mut cancelled = AssistantAccumulator::new(model());
    let events = cancelled.abort();
    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        ["start", "error"]
    );
    let AssistantMessageEvent::Error { reason, error } = &events[1] else {
        panic!("expected cancellation error");
    };
    assert_eq!(*reason, StopReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
}

#[test]
fn maps_all_genai_stop_reasons_and_keeps_the_raw_value() {
    let cases = [
        (GenaiStopReason::Completed("stop".into()), StopReason::Stop),
        (
            GenaiStopReason::MaxTokens("length".into()),
            StopReason::Length,
        ),
        (
            GenaiStopReason::ToolCall("tool_use".into()),
            StopReason::ToolUse,
        ),
        (
            GenaiStopReason::ContentFilter("SAFETY".into()),
            StopReason::Stop,
        ),
        (
            GenaiStopReason::StopSequence("STOP_SEQUENCE".into()),
            StopReason::Stop,
        ),
        (
            GenaiStopReason::Other("provider_value".into()),
            StopReason::Stop,
        ),
    ];

    for (upstream, expected) in cases {
        let raw = upstream.raw().to_string();
        let mut accumulator = AssistantAccumulator::new(model());
        let events = accumulator.fold(ChatStreamEvent::End(StreamEnd {
            captured_stop_reason: Some(upstream),
            ..Default::default()
        }));
        let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else {
            panic!("expected done");
        };
        assert_eq!(*reason, expected);
        assert_eq!(message.provider_stop_reason.as_deref(), Some(raw.as_str()));
    }
}

#[test]
fn tolerant_json_fold_handles_nested_eof_invalid_escapes_and_partial_literals() {
    assert_eq!(
        parse_streaming_json(r#"{"outer":{"items":[1,2,{"ok":tru"#),
        json!({"outer": {"items": [1, 2, {"ok": true}]}})
    );
    assert_eq!(
        parse_streaming_json(
            r#"{"path":"C:\invalid\q","line":"one
 two"#
        ),
        json!({"path": "C:\\invalid\\q", "line": "one\n two"})
    );
    assert_eq!(parse_streaming_json(r#"{"unfinished"#), json!({}));
    assert_eq!(parse_streaming_json(r#"{"a":1]"#), json!({"a": 1}));
}

#[test]
fn public_agent_tool_call_shape_remains_usable_in_fold_assertions() {
    let call = AgentToolCall::new("id", "name", json!({"x": 1}));
    assert_eq!(call.id, "id");
}

#[tokio::test]
async fn genai_stream_fn_turns_preflight_cancellation_into_an_in_band_abort() {
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let request =
        StreamRequest::new("openai::unused", LlmContext::default()).with_cancellation(cancel);
    let mut stream = GenaiStreamFn::new(Client::default()).stream(request).await;
    let events = stream.by_ref().collect::<Vec<_>>().await;

    assert_eq!(
        events.iter().map(event_name).collect::<Vec<_>>(),
        ["start", "error"]
    );
    let AssistantMessageEvent::Error { reason, error } = &events[1] else {
        panic!("expected abort");
    };
    assert_eq!(*reason, StopReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(
        stream.result().await.unwrap().stop_reason,
        StopReason::Aborted
    );
}
