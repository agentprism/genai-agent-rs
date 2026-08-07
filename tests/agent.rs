//! Case-for-case port of `pi/packages/agent/test/agent.test.ts`.
//!
//! All 21 substantive black-box cases were enabled and green at the M1-M6 checkpoint; the
//! reset-while-processing rejection case landed upstream afterwards and is ported here too.

#![cfg(feature = "testing")]
// Direct parity setup intentionally mirrors the upstream tests' incremental option mutation.
#![allow(clippy::field_reassign_with_default)]

use async_trait::async_trait;
use rust_genai_agent::testing::{EventRecorder, MockStreamFn, ScriptedStream, fixtures, script};
use rust_genai_agent::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentMessage, AgentPrepareNextTurnHook,
    AgentShouldStopAfterTurnHook, AgentState, AgentTool, AgentToolCall, AgentToolResult,
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, BusyContext,
    CancellationToken, EventKind, FnTool, StopReason, StreamFn, StreamRequest, ThinkingLevel,
    ToolResultContent, ToolSpec, UpdateSink, set_default_stream_fn,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

fn agent_with_streams(streams: Vec<ScriptedStream>) -> (Agent, Arc<MockStreamFn>) {
    let stream_fn = Arc::new(MockStreamFn::from_streams(streams));
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream_fn.clone()));
    (agent, stream_fn)
}

fn agent_with_state_and_streams(
    state: AgentState,
    streams: Vec<ScriptedStream>,
) -> (Agent, Arc<MockStreamFn>) {
    let stream_fn = Arc::new(MockStreamFn::from_streams(streams));
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(stream_fn.clone()),
    );
    (agent, stream_fn)
}

fn empty_tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        format!("{name} test tool"),
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    )
    .with_label(name.replace('_', " "))
}

fn tool_result(text: &str, status: &str, terminate: bool) -> AgentToolResult {
    AgentToolResult::new(
        vec![ToolResultContent::text(text)],
        json!({ "status": status }),
    )
    .with_terminate(terminate)
}

fn message_has_user_text(message: &AgentMessage, expected: &str) -> bool {
    let AgentMessage::User(user) = message else {
        return false;
    };
    user.content.iter().any(
        |part| matches!(part, rust_genai_agent::UserContent::Text { text } if text == expected),
    )
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition did not become true");
}

fn abort_aware_stream_fn() -> Arc<MockStreamFn> {
    Arc::new(MockStreamFn::from_fn(|request| {
        ScriptedStream::from_driver(move |sender| async move {
            let partial = AssistantMessage::new(fixtures::model_iden());
            sender
                .send(AssistantMessageEvent::Start { partial })
                .expect("agent consumes stream start");
            request.cancel.cancelled().await;
            let error =
                AssistantMessage::error(fixtures::model_iden(), StopReason::Aborted, "Aborted");
            let _ = sender.send(AssistantMessageEvent::Error {
                reason: StopReason::Aborted,
                error,
            });
        })
    }))
}

struct PanickingStreamFn;

#[async_trait]
impl StreamFn for PanickingStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        panic!("provider exploded");
    }
}

struct DefaultStreamRestore(Option<Arc<dyn StreamFn>>);

impl Drop for DefaultStreamRestore {
    fn drop(&mut self) {
        set_default_stream_fn(self.0.take());
    }
}

// TS: pi/packages/agent/test/agent.test.ts — `uses the configured default when a legacy caller omits streamFn`
#[tokio::test]
async fn legacy_omitted_stream_fn_uses_configured_default() {
    let fallback = Arc::new(MockStreamFn::from_messages(vec![fixtures::text_msg(
        "fallback",
    )]));
    let previous = set_default_stream_fn(Some(fallback.clone()));
    let _restore = DefaultStreamRestore(previous);

    // Idiomatic adaptation of `Reflect.construct(Agent, [{}])`: the default config leaves
    // `stream_fn` unset (`None`), so admission resolves the process default.
    let agent = Agent::new(AgentConfig::default());
    agent.prompt("Hello").await.unwrap();

    assert_eq!(fallback.call_count(), 1);
}

// TS: pi/packages/agent/test/agent.test.ts — `should create an agent instance with default state`
#[tokio::test]
async fn creates_agent_with_default_state() {
    let (agent, _) = agent_with_streams(vec![]);
    let state = agent.state();

    assert_eq!(state.system_prompt, "");
    assert!(matches!(state.model, rust_genai_agent::ModelSpec::Iden(_)));
    assert_eq!(state.thinking_level, ThinkingLevel::Off);
    assert!(state.tools.is_empty());
    assert!(state.messages.is_empty());
    assert!(!state.is_streaming);
    assert!(state.streaming_message.is_none());
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.error_message.is_none());
}

// TS: pi/packages/agent/test/agent.test.ts — `should create an agent instance with custom initial state`
#[tokio::test]
async fn creates_agent_with_custom_initial_state() {
    let custom_model = fixtures::model();
    let expected_model = format!("{custom_model:?}");
    let state = AgentState {
        system_prompt: "You are a helpful assistant.".into(),
        model: custom_model,
        thinking_level: ThinkingLevel::Low,
        ..AgentState::default()
    };
    let (agent, _) = agent_with_state_and_streams(state, vec![]);
    let state = agent.state();

    assert_eq!(state.system_prompt, "You are a helpful assistant.");
    assert_eq!(format!("{:?}", state.model), expected_model);
    assert_eq!(state.thinking_level, ThinkingLevel::Low);
}

// TS: pi/packages/agent/test/agent.test.ts — `should subscribe to events`
#[tokio::test]
async fn subscribes_and_unsubscribes_to_events() {
    let (agent, _) = agent_with_streams(vec![]);
    let count = Arc::new(Mutex::new(0usize));
    let observed = count.clone();
    let subscription = agent.subscribe_fn(move |_event, _cancel| {
        let observed = observed.clone();
        async move { *observed.lock().unwrap() += 1 }
    });

    assert_eq!(*count.lock().unwrap(), 0, "subscribe emits no snapshot");
    agent.set_system_prompt("Test prompt");
    assert_eq!(*count.lock().unwrap(), 0, "state setters emit no events");
    assert_eq!(agent.state().system_prompt, "Test prompt");

    subscription.unsubscribe();
    agent.set_system_prompt("Another prompt");
    assert_eq!(*count.lock().unwrap(), 0, "unsubscribe is immediate");
}

// TS: pi/packages/agent/test/agent.test.ts — `emits full lifecycle events for thrown run failures`
#[tokio::test]
async fn thrown_run_failure_emits_full_lifecycle() {
    let agent = Agent::new(AgentConfig::default().with_stream_fn(Arc::new(PanickingStreamFn)));
    let events = EventRecorder::new();
    let _subscription = agent.subscribe(events.listener());

    // Provider panics are adversarial violations of StreamFn's never-throw contract;
    // the stateful boundary still catches them and synthesizes a complete failed turn.
    agent.prompt("hello").await.unwrap();

    events.assert_sequence(&[
        EventKind::AgentStart,
        EventKind::TurnStart,
        EventKind::MessageStart,
        EventKind::MessageEnd,
        EventKind::MessageStart,
        EventKind::MessageEnd,
        EventKind::TurnEnd,
        EventKind::AgentEnd,
    ]);
    let state = agent.state();
    let AgentMessage::Assistant(last) = state.messages.last().expect("failure assistant message")
    else {
        panic!("expected assistant message");
    };
    assert_eq!(last.stop_reason, StopReason::Error);
    assert_eq!(last.error_message.as_deref(), Some("provider exploded"));
    assert_eq!(state.error_message.as_deref(), Some("provider exploded"));
}

// Parity: agent.ts:570-572 — TS truthiness means an empty-string errorMessage on turn_end does
// not populate state.errorMessage.
#[tokio::test]
async fn empty_error_message_does_not_populate_state() {
    let (agent, _) = agent_with_streams(vec![script::in_band_error("")]);

    agent.prompt("hello").await.unwrap();

    let state = agent.state();
    let AgentMessage::Assistant(last) = state.messages.last().expect("assistant terminal message")
    else {
        panic!("expected assistant message");
    };
    assert_eq!(last.stop_reason, StopReason::Error);
    assert_eq!(last.error_message.as_deref(), Some(""));
    assert!(state.error_message.is_none());
}

// TS: pi/packages/agent/test/agent.test.ts — `should await async subscribers before prompt resolves`
#[tokio::test]
async fn prompt_awaits_async_subscribers() {
    let (agent, _) = agent_with_streams(vec![script::text_response("ok")]);
    let barrier = Arc::new(Semaphore::new(0));
    let listener_finished = Arc::new(AtomicBool::new(false));
    let listener_barrier = barrier.clone();
    let finished = listener_finished.clone();
    let _subscription = agent.subscribe_fn(move |event, _cancel| {
        let listener_barrier = listener_barrier.clone();
        let finished = finished.clone();
        async move {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                let permit = listener_barrier.acquire().await.unwrap();
                permit.forget();
                finished.store(true, Ordering::SeqCst);
            }
        }
    });

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("hello").await });
    wait_until(|| agent.state().is_streaming).await;
    tokio::task::yield_now().await;

    assert!(!prompt.is_finished());
    assert!(!listener_finished.load(Ordering::SeqCst));
    assert!(agent.state().is_streaming);

    barrier.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(listener_finished.load(Ordering::SeqCst));
    assert!(!agent.state().is_streaming);
}

// TS: pi/packages/agent/test/agent.test.ts — `waitForIdle should wait for async subscribers`
#[tokio::test]
async fn wait_for_idle_awaits_async_subscribers() {
    let (agent, _) = agent_with_streams(vec![script::text_response("ok")]);
    let barrier = Arc::new(Semaphore::new(0));
    let listener_barrier = barrier.clone();
    let _subscription = agent.subscribe_fn(move |event, _cancel| {
        let listener_barrier = listener_barrier.clone();
        async move {
            if matches!(
                event,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_)
                }
            ) {
                let permit = listener_barrier.acquire().await.unwrap();
                permit.forget();
            }
        }
    });

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("hello").await });
    wait_until(|| agent.state().is_streaming).await;

    let idle_resolved = Arc::new(AtomicBool::new(false));
    let resolved = idle_resolved.clone();
    let waiting_agent = agent.clone();
    let idle = tokio::spawn(async move {
        waiting_agent.wait_for_idle().await;
        resolved.store(true, Ordering::SeqCst);
    });
    tokio::task::yield_now().await;

    assert!(!idle_resolved.load(Ordering::SeqCst));
    assert!(agent.state().is_streaming);

    barrier.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        prompt.await.unwrap().unwrap();
        idle.await.unwrap();
    })
    .await
    .unwrap();
    assert!(idle_resolved.load(Ordering::SeqCst));
    assert!(!agent.state().is_streaming);
}

// TS: pi/packages/agent/test/agent.test.ts — `should pass the active abort signal to subscribers`
#[tokio::test]
async fn subscribers_receive_active_cancellation_token() {
    let stream_fn = abort_aware_stream_fn();
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream_fn));
    let received = Arc::new(Mutex::new(None::<CancellationToken>));
    let observed = received.clone();
    let _subscription = agent.subscribe_fn(move |event, cancel| {
        let observed = observed.clone();
        async move {
            if matches!(event, AgentEvent::AgentStart) {
                *observed.lock().unwrap() = Some(cancel);
            }
        }
    });

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("hello").await });
    wait_until(|| received.lock().unwrap().is_some()).await;
    let token = received.lock().unwrap().clone().unwrap();
    assert!(!token.is_cancelled());

    agent.abort();
    tokio::time::timeout(Duration::from_secs(2), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(token.is_cancelled());
}

// TS: pi/packages/agent/test/agent.test.ts — `should ignore tool updates after the tool execution settles`
#[tokio::test]
async fn ignores_tool_updates_after_execution_settles() {
    let captured_update = Arc::new(Mutex::new(None::<UpdateSink>));
    let update_slot = captured_update.clone();
    let tool: Arc<dyn AgentTool> = Arc::new(FnTool::new(
        empty_tool_spec("delayed_tool"),
        move |_call, _cancel, on_update| {
            let update_slot = update_slot.clone();
            async move {
                *update_slot.lock().unwrap() = Some(on_update.clone());
                assert!(on_update.emit(tool_result("running", "running", false)));
                Ok(tool_result("ok", "done", true))
            }
        },
    ));
    let mut state = AgentState::default();
    state.tools = vec![tool];
    let (agent, _) = agent_with_state_and_streams(
        state,
        vec![script::tool_call_turn(vec![AgentToolCall::new(
            "call-1",
            "delayed_tool",
            json!({}),
        )])],
    );
    let events = EventRecorder::new();
    let _subscription = agent.subscribe(events.listener());

    agent.prompt("run tool").await.unwrap();
    let count_after_prompt = events.events().len();
    let update = captured_update
        .lock()
        .unwrap()
        .clone()
        .expect("tool captured its scoped update sink");
    assert!(!update.emit(tool_result("late", "late", false)));
    tokio::task::yield_now().await;

    assert_eq!(
        events
            .events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
            .count(),
        1
    );
    assert_eq!(events.events().len(), count_after_prompt);
}

// TS: pi/packages/agent/test/agent.test.ts — `should ignore a settled parallel tool update while another tool is still running`
#[tokio::test]
async fn ignores_settled_parallel_tool_update_while_other_tool_runs() {
    let captured_update = Arc::new(Mutex::new(None::<UpdateSink>));
    let update_slot = captured_update.clone();
    let settled_tool: Arc<dyn AgentTool> = Arc::new(FnTool::new(
        empty_tool_spec("settled_tool"),
        move |_call, _cancel, on_update| {
            let update_slot = update_slot.clone();
            async move {
                *update_slot.lock().unwrap() = Some(on_update);
                Ok(tool_result("done", "done", true))
            }
        },
    ));

    let slow_started = Arc::new(Semaphore::new(0));
    let release_slow = Arc::new(Semaphore::new(0));
    let started = slow_started.clone();
    let release = release_slow.clone();
    let slow_tool: Arc<dyn AgentTool> = Arc::new(FnTool::new(
        empty_tool_spec("slow_tool"),
        move |_call, _cancel, _on_update| {
            let started = started.clone();
            let release = release.clone();
            async move {
                started.add_permits(1);
                let permit = release.acquire().await.unwrap();
                permit.forget();
                Ok(tool_result("done", "done", true))
            }
        },
    ));

    let mut state = AgentState::default();
    state.tools = vec![settled_tool, slow_tool];
    let (agent, _) = agent_with_state_and_streams(
        state,
        vec![script::tool_call_turn(vec![
            AgentToolCall::new("call-1", "settled_tool", json!({})),
            AgentToolCall::new("call-2", "slow_tool", json!({})),
        ])],
    );
    let events = EventRecorder::new();
    let _subscription = agent.subscribe(events.listener());

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("run tools").await });
    let permit = tokio::time::timeout(Duration::from_secs(2), slow_started.acquire())
        .await
        .unwrap()
        .unwrap();
    permit.forget();
    wait_until(|| {
        events.events().iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call-1"
            )
        })
    })
    .await;
    let event_count_before_late_update = events.events().len();

    let update = captured_update
        .lock()
        .unwrap()
        .clone()
        .expect("settled tool captured update sink");
    assert!(!update.emit(tool_result("late", "late", false)));
    tokio::task::yield_now().await;
    assert_eq!(events.events().len(), event_count_before_late_update);

    release_slow.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        events
            .events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
            .count(),
        0
    );
}

// TS: pi/packages/agent/test/agent.test.ts — `should update state with mutators`
#[tokio::test]
async fn state_mutators_replace_and_copy() {
    let (agent, _) = agent_with_streams(vec![]);

    agent.set_system_prompt("Custom prompt");
    assert_eq!(agent.state().system_prompt, "Custom prompt");

    let model = fixtures::model();
    let expected_model = format!("{model:?}");
    agent.set_model(model);
    assert_eq!(format!("{:?}", agent.state().model), expected_model);

    agent.set_thinking_level(ThinkingLevel::High);
    assert_eq!(agent.state().thinking_level, ThinkingLevel::High);

    let tool: Arc<dyn AgentTool> = Arc::new(FnTool::from_value_fn(
        empty_tool_spec("test"),
        |_args| async { Ok(AgentToolResult::text("ok")) },
    ));
    let mut caller_tools = vec![tool];
    agent.set_tools(caller_tools.clone());
    caller_tools.clear();
    assert_eq!(agent.state().tools.len(), 1, "setter owns a top-level copy");

    let mut caller_messages = vec![AgentMessage::user("Hello")];
    agent.set_messages(caller_messages.clone());
    caller_messages.clear();
    assert_eq!(agent.state().messages, vec![AgentMessage::user("Hello")]);

    let appended = AgentMessage::Assistant(fixtures::text_msg("Hi"));
    let mut replacement = agent.state().messages;
    replacement.push(appended.clone());
    agent.set_messages(replacement);
    assert_eq!(agent.state().messages.len(), 2);
    assert_eq!(agent.state().messages[1], appended);

    agent.set_messages(vec![]);
    assert!(agent.state().messages.is_empty());
}

// TS: pi/packages/agent/test/agent.test.ts — `should support steering message queue`
#[tokio::test]
async fn supports_steering_message_queue() {
    let (agent, _) = agent_with_streams(vec![]);
    let message = AgentMessage::user("Steering message");

    agent.steer(message.clone());

    assert!(!agent.state().messages.contains(&message));
    assert!(agent.has_queued_messages());
    agent.clear_steering_queue();
    assert!(!agent.has_queued_messages());
}

// TS: pi/packages/agent/test/agent.test.ts — `should support follow-up message queue`
#[tokio::test]
async fn supports_follow_up_message_queue() {
    let (agent, _) = agent_with_streams(vec![]);
    let message = AgentMessage::user("Follow-up message");

    agent.follow_up(message.clone());

    assert!(!agent.state().messages.contains(&message));
    assert!(agent.has_queued_messages());
    agent.clear_follow_up_queue();
    assert!(!agent.has_queued_messages());
}

// TS: pi/packages/agent/test/agent.test.ts — `should handle abort controller`
#[tokio::test]
async fn abort_is_safe_while_idle() {
    let (agent, _) = agent_with_streams(vec![]);
    assert!(agent.signal().is_none());
    agent.abort();
    assert!(agent.signal().is_none());
    assert!(!agent.state().is_streaming);
}

// TS: pi/packages/agent/test/agent.test.ts — `should reject reset while processing without corrupting the transcript`
#[tokio::test]
async fn reset_rejects_while_processing_without_corrupting_the_transcript() {
    let release = Arc::new(Semaphore::new(0));
    let release_for_stream = release.clone();
    let stream_fn = Arc::new(MockStreamFn::from_fn(move |_request| {
        let release = release_for_stream.clone();
        ScriptedStream::from_driver(move |sender| async move {
            let partial = AssistantMessage::new(fixtures::model_iden());
            sender
                .send(AssistantMessageEvent::Start { partial })
                .expect("agent consumes stream start");
            let permit = release.acquire().await.unwrap();
            permit.forget();
            let _ = sender.send(AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: fixtures::text_msg("Done"),
            });
        })
    }));
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream_fn));

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("Hello").await });
    wait_until(|| agent.state().is_streaming).await;

    let error = agent.reset().unwrap_err();
    assert!(matches!(error, AgentError::Busy(BusyContext::Reset)));
    let state = agent.state();
    assert!(state.is_streaming);
    assert_eq!(
        state
            .messages
            .iter()
            .map(|message| message.role())
            .collect::<Vec<_>>(),
        ["user"]
    );

    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(
        state
            .messages
            .iter()
            .map(|message| message.role())
            .collect::<Vec<_>>(),
        ["user", "assistant"]
    );
}

// TS: pi/packages/agent/test/agent.test.ts — `should throw when prompt() called while streaming`
#[tokio::test]
async fn prompt_rejects_while_streaming() {
    let stream_fn = abort_aware_stream_fn();
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream_fn));
    let running_agent = agent.clone();
    let first_prompt = tokio::spawn(async move { running_agent.prompt("First message").await });
    wait_until(|| agent.state().is_streaming).await;

    let error = agent.prompt("Second message").await.unwrap_err();
    assert!(matches!(error, AgentError::Busy(BusyContext::Prompt)));

    agent.abort();
    tokio::time::timeout(Duration::from_secs(2), first_prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// TS: pi/packages/agent/test/agent.test.ts — `should throw when continue() called while streaming`
#[tokio::test]
async fn continue_rejects_while_streaming() {
    let stream_fn = abort_aware_stream_fn();
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream_fn));
    let running_agent = agent.clone();
    let first_prompt = tokio::spawn(async move { running_agent.prompt("First message").await });
    wait_until(|| agent.state().is_streaming).await;

    let error = agent.continue_().await.unwrap_err();
    assert!(matches!(error, AgentError::Busy(BusyContext::Continue)));

    agent.abort();
    tokio::time::timeout(Duration::from_secs(2), first_prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// Parity: agent.ts:335 (reset), agent.ts:353 (prompt), agent.ts:363 (continue), agent.ts:488
// (guarded setters) — the four site-specific busy texts, byte for byte.
#[test]
fn busy_error_texts_match_the_typescript_strings() {
    assert_eq!(
        AgentError::Busy(BusyContext::Prompt).to_string(),
        "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
    );
    assert_eq!(
        AgentError::Busy(BusyContext::Continue).to_string(),
        "Agent is already processing. Wait for completion before continuing."
    );
    assert_eq!(
        AgentError::Busy(BusyContext::Reset).to_string(),
        "Agent is already processing. Wait for completion before resetting."
    );
    assert_eq!(
        AgentError::Busy(BusyContext::Other).to_string(),
        "Agent is already processing."
    );
}

// TS: pi/packages/agent/test/agent.test.ts — `continue() should process queued follow-up messages after an assistant turn`
#[tokio::test]
async fn continue_processes_queued_follow_up_after_assistant_turn() {
    let mut state = AgentState::default();
    state.messages = vec![
        AgentMessage::user("Initial"),
        AgentMessage::Assistant(fixtures::text_msg("Initial response")),
    ];
    let (agent, _) = agent_with_state_and_streams(state, vec![script::text_response("Processed")]);
    agent.follow_up(AgentMessage::user("Queued follow-up"));

    agent.continue_().await.unwrap();

    let state = agent.state();
    assert!(
        state
            .messages
            .iter()
            .any(|message| message_has_user_text(message, "Queued follow-up"))
    );
    assert!(matches!(
        state.messages.last(),
        Some(AgentMessage::Assistant(_))
    ));
    assert!(!agent.has_queued_messages());
}

// TS: pi/packages/agent/test/agent.test.ts — `continue() should keep one-at-a-time steering semantics from assistant tail`
#[tokio::test]
async fn continue_preserves_one_at_a_time_steering_from_assistant_tail() {
    let mut state = AgentState::default();
    state.messages = vec![
        AgentMessage::user("Initial"),
        AgentMessage::Assistant(fixtures::text_msg("Initial response")),
    ];
    let (agent, stream_fn) = agent_with_state_and_streams(
        state,
        vec![
            script::text_response("Processed 1"),
            script::text_response("Processed 2"),
        ],
    );
    agent.steer(AgentMessage::user("Steering 1"));
    agent.steer(AgentMessage::user("Steering 2"));

    agent.continue_().await.unwrap();

    let state = agent.state();
    let roles: Vec<_> = state
        .messages
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|message| message.role())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(stream_fn.call_count(), 2);
    assert!(!agent.has_queued_messages());
}

// TS: pi/packages/agent/test/agent.test.ts — `keeps legacy prepareNextTurn signal callback behavior`
#[tokio::test]
async fn forwards_legacy_prepare_next_turn_cancellation_token() {
    let noop: Arc<dyn AgentTool> = Arc::new(FnTool::from_value_fn(
        empty_tool_spec("noop"),
        |_args| async { Ok(AgentToolResult::text("ok")) },
    ));
    let saw_active_token = Arc::new(AtomicBool::new(false));
    let saw_token = saw_active_token.clone();
    let prepare_next_turn: AgentPrepareNextTurnHook = Arc::new(move |cancel| {
        saw_token.store(!cancel.is_cancelled(), Ordering::SeqCst);
        Box::pin(async { None })
    });
    let stream_fn = Arc::new(MockStreamFn::from_streams(vec![
        script::tool_call_turn(vec![AgentToolCall::new("tool-1", "noop", json!({}))]),
        script::text_response("done"),
    ]));
    let mut state = AgentState::default();
    state.tools = vec![noop];
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(stream_fn.clone())
            .with_prepare_next_turn(prepare_next_turn),
    );

    agent.prompt("start").await.unwrap();

    assert_eq!(stream_fn.call_count(), 2);
    assert!(saw_active_token.load(Ordering::SeqCst));
}

// TS: pi/packages/agent/test/agent.test.ts — `forwards shouldStopAfterTurn through AgentOptions`
#[tokio::test]
async fn forwards_should_stop_after_turn_through_options() {
    let noop: Arc<dyn AgentTool> = Arc::new(FnTool::from_value_fn(
        empty_tool_spec("noop"),
        |_args| async { Ok(AgentToolResult::text("tool complete")) },
    ));
    let saw_active_token = Arc::new(AtomicBool::new(false));
    let saw_token = saw_active_token.clone();
    let context_roles = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_roles = context_roles.clone();
    let should_stop: AgentShouldStopAfterTurnHook = Arc::new(move |context, cancel| {
        saw_token.store(!cancel.is_cancelled(), Ordering::SeqCst);
        *observed_roles.lock().unwrap() = context
            .context
            .messages
            .iter()
            .map(|message| message.role().to_owned())
            .collect();
        Box::pin(async { true })
    });
    let stream_fn = Arc::new(MockStreamFn::from_streams(vec![
        script::tool_call_turn(vec![AgentToolCall::new("tool-1", "noop", json!({}))]),
        script::text_response("should not run"),
    ]));
    let mut state = AgentState::default();
    state.tools = vec![noop];
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(stream_fn.clone())
            .with_should_stop_after_turn(should_stop),
    );

    agent.prompt("start").await.unwrap();

    assert_eq!(stream_fn.call_count(), 1);
    assert!(saw_active_token.load(Ordering::SeqCst));
    assert_eq!(
        *context_roles.lock().unwrap(),
        ["user", "assistant", "tool_result"]
    );
}

// TS: pi/packages/agent/test/agent.test.ts — `forwards sessionId to streamFunction options`
#[tokio::test]
async fn forwards_session_id_to_stream_options() {
    let stream_fn = Arc::new(MockStreamFn::from_streams(vec![
        script::text_response("ok"),
        script::text_response("ok again"),
    ]));
    let agent = Agent::new(
        AgentConfig::default()
            .with_stream_fn(stream_fn.clone())
            .with_session_id("session-abc"),
    );

    agent.prompt("hello").await.unwrap();
    assert_eq!(
        stream_fn.calls()[0].options.prompt_cache_key.as_deref(),
        Some("session-abc")
    );
    assert_eq!(agent.session_id().as_deref(), Some("session-abc"));

    agent.set_session_id(Some("session-def".into()));
    assert_eq!(agent.session_id().as_deref(), Some("session-def"));
    agent.prompt("hello again").await.unwrap();
    assert_eq!(
        stream_fn.calls()[1].options.prompt_cache_key.as_deref(),
        Some("session-def")
    );
}
