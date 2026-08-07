//! Scripted end-to-end parity for `pi/packages/agent/test/e2e.test.ts`.
//!
//! These 10 green cases exercise `Agent`, not the low-level loop. `ScriptedStream` and
//! `MockStreamFn` replace pi-ai's faux provider, so the suite never calls a live provider.

#![cfg(feature = "testing")]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_genai_agent::testing::{MockStreamFn, ScriptedStream, fixtures, script, tools};
use rust_genai_agent::{
    Agent, AgentConfig, AgentEvent, AgentListener, AgentMessage, AgentState, AgentTool,
    AssistantContent, StopReason, ThinkingLevel,
};
use serde_json::json;

fn scripted(message: rust_genai_agent::AssistantMessage) -> ScriptedStream {
    ScriptedStream::from_message(message)
}

fn test_agent(
    system_prompt: &str,
    thinking_level: ThinkingLevel,
    tools: Vec<Arc<dyn AgentTool>>,
    streams: Vec<ScriptedStream>,
) -> (Arc<Agent>, Arc<MockStreamFn>) {
    let stream_fn = Arc::new(MockStreamFn::from_streams(streams));
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(AgentState {
                system_prompt: system_prompt.to_owned(),
                model: fixtures::model(),
                thinking_level,
                tools,
                messages: Vec::new(),
                is_streaming: false,
                streaming_message: None,
                pending_tool_calls: HashSet::new(),
                error_message: None,
            })
            .with_stream_fn(stream_fn.clone()),
    );
    (Arc::new(agent), stream_fn)
}

fn text_content(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Assistant(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::ToolResult(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                rust_genai_agent::ToolResultContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

// TS: pi/packages/agent/test/e2e.test.ts — "handles a basic text prompt"
#[tokio::test]
async fn handles_a_basic_text_prompt() {
    let (agent, _) = test_agent(
        "You are a helpful assistant. Keep your responses concise.",
        ThinkingLevel::Off,
        vec![],
        vec![scripted(fixtures::assistant_msg(
            vec![script::text("4")],
            StopReason::Stop,
        ))],
    );

    agent
        .prompt("What is 2+2? Answer with just the number.")
        .await
        .unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(&state.messages[0], AgentMessage::User(_)));
    assert!(matches!(&state.messages[1], AgentMessage::Assistant(_)));
    assert!(text_content(&state.messages[1]).contains('4'));
}

// TS: pi/packages/agent/test/e2e.test.ts — "executes tools and tracks pending tool calls"
#[tokio::test]
async fn executes_tools_and_tracks_pending_tool_calls() {
    let tool_turn = fixtures::assistant_msg(
        vec![
            script::text("Let me calculate that."),
            script::tool_call("calc-1", "calculate", json!({ "expression": "123 * 456" })),
        ],
        StopReason::ToolUse,
    );
    let final_turn =
        fixtures::assistant_msg(vec![script::text("The result is 56088.")], StopReason::Stop);
    let (agent, _) = test_agent(
        "You are a helpful assistant. Always use the calculator tool for math.",
        ThinkingLevel::Off,
        vec![tools::calculate_tool()],
        vec![scripted(tool_turn), scripted(final_turn)],
    );

    let pending_during_events = Arc::new(Mutex::new(Vec::<(&'static str, Vec<String>)>::new()));
    let observations = pending_during_events.clone();
    let observed_agent = Arc::downgrade(&agent);
    let listener: AgentListener = Arc::new(move |event, _cancel| {
        if matches!(
            &event,
            AgentEvent::ToolExecutionStart { .. } | AgentEvent::ToolExecutionEnd { .. }
        ) {
            let agent = observed_agent.upgrade().expect("agent remains alive");
            let mut ids: Vec<_> = agent.state().pending_tool_calls.into_iter().collect();
            ids.sort();
            observations.lock().unwrap().push((event_name(&event), ids));
        }
        Box::pin(async {})
    });
    let _subscription = agent.subscribe(listener);

    agent
        .prompt("Calculate 123 * 456 using the calculator tool.")
        .await
        .unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 4);
    let result = state
        .messages
        .iter()
        .find(|message| matches!(message, AgentMessage::ToolResult(_)))
        .expect("a tool-result message");
    assert!(text_content(result).contains("123 * 456 = 56088"));
    let final_message = state.messages.last().expect("a final assistant message");
    assert!(matches!(final_message, AgentMessage::Assistant(_)));
    assert!(text_content(final_message).contains("56088"));
    assert!(state.pending_tool_calls.is_empty());
    assert_eq!(
        *pending_during_events.lock().unwrap(),
        vec![
            ("tool_execution_start", vec!["calc-1".to_owned()]),
            ("tool_execution_end", vec![]),
        ]
    );
}

// TS: pi/packages/agent/test/e2e.test.ts — "handles abort during streaming"
#[tokio::test]
async fn handles_abort_during_streaming() {
    let response = fixtures::assistant_msg(
        vec![script::text(
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen",
        )],
        StopReason::Stop,
    );
    let delayed = ScriptedStream::from_message(response).with_delay(Duration::from_millis(50));
    let (agent, _) = test_agent(
        "You are a helpful assistant.",
        ThinkingLevel::Off,
        vec![],
        vec![delayed],
    );

    let running_agent = agent.clone();
    let prompt =
        tokio::spawn(async move { running_agent.prompt("Count slowly from 1 to 20.").await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    agent.abort();
    prompt.await.unwrap().unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 2);
    let last_message = state.messages.last().expect("an assistant message");
    let AgentMessage::Assistant(last_message) = last_message else {
        panic!("expected assistant message");
    };
    assert_eq!(last_message.stop_reason, StopReason::Aborted);
    assert!(last_message.error_message.is_some());
    assert_eq!(state.error_message, last_message.error_message);
}

// TS: pi/packages/agent/test/e2e.test.ts — "emits lifecycle updates while streaming"
#[tokio::test]
async fn emits_lifecycle_updates_while_streaming() {
    let response = fixtures::assistant_msg(vec![script::text("1 2 3 4 5")], StopReason::Stop);
    let stream = ScriptedStream::from_message(response);
    let (agent, _) = test_agent(
        "You are a helpful assistant.",
        ThinkingLevel::Off,
        vec![],
        vec![stream],
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let listener: AgentListener = Arc::new(move |event, _cancel| {
        recorded.lock().unwrap().push(event_name(&event));
        Box::pin(async {})
    });
    let _subscription = agent.subscribe(listener);

    agent.prompt("Count from 1 to 5.").await.unwrap();

    let events = events.lock().unwrap();
    for required in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_update",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(events.contains(&required), "missing {required}: {events:?}");
    }
    let first = |name| events.iter().position(|event| *event == name).unwrap();
    let last = |name| events.iter().rposition(|event| *event == name).unwrap();
    assert!(first("agent_start") < first("message_start"));
    assert!(first("message_start") < first("message_end"));
    assert!(first("message_end") < last("agent_end"));
    drop(events);

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
}

// TS: pi/packages/agent/test/e2e.test.ts — "maintains context across multiple turns"
#[tokio::test]
async fn maintains_context_across_multiple_turns() {
    let (agent, stream_fn) = test_agent(
        "You are a helpful assistant.",
        ThinkingLevel::Off,
        vec![],
        vec![
            scripted(fixtures::assistant_msg(
                vec![script::text("Nice to meet you, Alice.")],
                StopReason::Stop,
            )),
            scripted(fixtures::assistant_msg(
                vec![script::text("Your name is Alice.")],
                StopReason::Stop,
            )),
        ],
    );

    agent.prompt("My name is Alice.").await.unwrap();
    assert_eq!(agent.state().messages.len(), 2);

    agent.prompt("What is my name?").await.unwrap();
    let state = agent.state();
    assert_eq!(state.messages.len(), 4);
    assert!(
        text_content(&state.messages[3])
            .to_lowercase()
            .contains("alice")
    );

    let calls = stream_fn.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        format!("{:?}", calls[1].context).contains("Alice"),
        "the second model context must contain the first user turn: {:?}",
        calls[1].context
    );
}

// TS: pi/packages/agent/test/e2e.test.ts — "preserves thinking content blocks"
#[tokio::test]
async fn preserves_thinking_content_blocks() {
    let expected = vec![script::thinking("step by step"), script::text("4")];
    let (agent, _) = test_agent(
        "You are a helpful assistant.",
        ThinkingLevel::Low,
        vec![],
        vec![scripted(fixtures::assistant_msg(
            expected.clone(),
            StopReason::Stop,
        ))],
    );

    agent.prompt("What is 2+2?").await.unwrap();

    let state = agent.state();
    let AgentMessage::Assistant(message) = &state.messages[1] else {
        panic!("expected assistant message");
    };
    assert_eq!(message.content, expected);
}

// TS: pi/packages/agent/test/e2e.test.ts — "throws when no messages in context"
#[tokio::test]
async fn continue_throws_when_no_messages_in_context() {
    let (agent, _) = test_agent("Test", ThinkingLevel::Off, vec![], vec![]);

    let error = agent.continue_().await.unwrap_err();

    assert_eq!(error.to_string(), "No messages to continue from");
}

// TS: pi/packages/agent/test/e2e.test.ts — "throws when last message is assistant"
#[tokio::test]
async fn continue_throws_when_last_message_is_assistant() {
    let (agent, _) = test_agent("Test", ThinkingLevel::Off, vec![], vec![]);
    agent.set_messages(vec![AgentMessage::Assistant(fixtures::assistant_msg(
        vec![script::text("Hello")],
        StopReason::Stop,
    ))]);

    let error = agent.continue_().await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "Cannot continue from message role: assistant"
    );
}

// TS: pi/packages/agent/test/e2e.test.ts — "continues and gets a response when last message is user"
#[tokio::test]
async fn continues_and_gets_a_response_when_last_message_is_user() {
    let (agent, _) = test_agent(
        "You are a helpful assistant. Follow instructions exactly.",
        ThinkingLevel::Off,
        vec![],
        vec![scripted(fixtures::assistant_msg(
            vec![script::text("HELLO WORLD")],
            StopReason::Stop,
        ))],
    );
    agent.set_messages(vec![fixtures::user_msg("Say exactly: HELLO WORLD")]);

    agent.continue_().await.unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(&state.messages[0], AgentMessage::User(_)));
    assert!(matches!(&state.messages[1], AgentMessage::Assistant(_)));
    assert!(
        text_content(&state.messages[1])
            .to_uppercase()
            .contains("HELLO WORLD")
    );
}

// TS: pi/packages/agent/test/e2e.test.ts — "continues and processes tool results"
#[tokio::test]
async fn continues_and_processes_tool_results() {
    let (agent, _) = test_agent(
        "You are a helpful assistant. After getting a calculation result, state the answer clearly.",
        ThinkingLevel::Off,
        vec![tools::calculate_tool()],
        vec![scripted(fixtures::assistant_msg(
            vec![script::text("The answer is 8.")],
            StopReason::Stop,
        ))],
    );
    let user = fixtures::user_msg("What is 5 + 3?");
    let assistant = AgentMessage::Assistant(fixtures::assistant_msg(
        vec![
            script::text("Let me calculate that."),
            script::tool_call("calc-1", "calculate", json!({ "expression": "5 + 3" })),
        ],
        StopReason::ToolUse,
    ));
    let tool_result = fixtures::tool_result_msg("calc-1", "calculate", "5 + 3 = 8");
    agent.set_messages(vec![user, assistant, tool_result]);

    agent.continue_().await.unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert!(state.messages.len() >= 4);
    let last_message = state.messages.last().expect("a final assistant message");
    assert!(matches!(last_message, AgentMessage::Assistant(_)));
    assert!(text_content(last_message).contains('8'));
}
