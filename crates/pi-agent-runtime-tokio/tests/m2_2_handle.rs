use futures_util::stream;
use pi_agent_core::{
    Agent, AgentEvent, AgentRecord, AgentState, MessageRole, RunOutcome, ToolRegistry,
};
use pi_agent_runtime_tokio::{AgentEventSink, TokioAgentHandle};
use pi_ai::{
    ApiId, AssistantEvent, AssistantFinish, AssistantFinishReason, AssistantMessage,
    AssistantStream, CancellationToken, ContentBlock, ContentBlockId, Message, MessageId, ModelId,
    ModelRef, ModelRequest, ModelRuntime, OpaquePayload, ProviderId, PublicError, ReasoningLevel,
    ReplayApplicability, ReplayCompleteness, ReplayEnvelope, ReplayItem, ReplayItemId, ReplayKind,
    ReplayScope, ReplayTarget, RequestStartError, RequestStartErrorKind, ScriptedRuntime,
    SendBoxFuture, Timestamp, Usage, UsageSource, UserMessage, text_response,
};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

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

fn sink(
    callback: impl Fn(AgentEvent, CancellationToken) -> SendBoxFuture<'static, ()>
    + Send
    + Sync
    + 'static,
) -> Arc<dyn AgentEventSink> {
    Arc::new(callback)
}

#[derive(Clone)]
struct TerminalOnlyRuntime {
    events: Arc<Mutex<VecDeque<AssistantEvent>>>,
}

impl TerminalOnlyRuntime {
    fn new(events: impl IntoIterator<Item = AssistantEvent>) -> Self {
        Self {
            events: Arc::new(Mutex::new(events.into_iter().collect())),
        }
    }

    fn remaining(&self) -> usize {
        lock(&self.events).len()
    }
}

impl ModelRuntime for TerminalOnlyRuntime {
    fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>> {
        Box::pin(async move {
            let event = lock(&self.events).pop_front().ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    "terminal-only runtime has no remaining response",
                )
            })?;
            Ok(AssistantStream::new(stream::iter([event])))
        })
    }
}

fn terminal_message(id: &str, reason: AssistantFinishReason) -> AssistantMessage {
    let provider = ProviderId::new("scripted");
    let api = ApiId::new("scripted");
    let model = ModelId::new("test-model");
    let error = match reason {
        AssistantFinishReason::Error => Some(PublicError {
            code: "provider_error".into(),
            message: "provider failed".into(),
            retryable: true,
            provider_code: Some("overloaded".into()),
            status: Some(503),
            request_id: Some("request-1".into()),
        }),
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
        end_turn: None,
        diagnostics: Vec::new(),
        content: Vec::new(),
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

fn terminal_event(id: &str, reason: AssistantFinishReason) -> AssistantEvent {
    let message = terminal_message(id, reason);
    match reason {
        AssistantFinishReason::Error => AssistantEvent::Failed { message },
        AssistantFinishReason::Aborted => AssistantEvent::Cancelled { message },
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => AssistantEvent::Finished { message },
    }
}

async fn assert_terminal_only_through_handle(
    event: AssistantEvent,
    expected: AssistantFinishReason,
) {
    let runtime = TerminalOnlyRuntime::new([event]);
    let agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    let handle = TokioAgentHandle::new(agent).unwrap();
    let run = handle
        .prompt_records([user("terminal-user", "prompt")])
        .await
        .unwrap();
    let outcome = run.outcome().await.unwrap();
    assert!(matches!(
        (expected, outcome),
        (AssistantFinishReason::Stop, RunOutcome::Completed { .. })
            | (AssistantFinishReason::Error, RunOutcome::Failed { .. })
            | (AssistantFinishReason::Aborted, RunOutcome::Cancelled { .. })
    ));
    let snapshot = handle.snapshot();
    let AgentRecord::Llm(Message::Assistant(message)) = snapshot.state.transcript.last().unwrap()
    else {
        panic!("terminal-only run must commit an assistant");
    };
    assert_eq!(message.finish.reason, expected);
    handle.shutdown().await.unwrap();
}

async fn assert_malformed_terminal_only_through_handle(event: AssistantEvent) {
    let expected_id = event.terminal_message().unwrap().id.clone();
    let runtime = TerminalOnlyRuntime::new([event]);
    let agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    let handle = TokioAgentHandle::new(agent).unwrap();
    let run = handle
        .prompt_records([user("malformed-terminal-user", "prompt")])
        .await
        .unwrap();
    assert!(matches!(
        run.outcome().await.unwrap(),
        RunOutcome::Failed { .. }
    ));
    let snapshot = handle.snapshot();
    let AgentRecord::Llm(Message::Assistant(message)) = snapshot.state.transcript.last().unwrap()
    else {
        panic!("malformed terminal-only run must commit a protocol-failure assistant");
    };
    assert_eq!(message.id, expected_id);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        message
            .finish
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("assistant_stream_protocol")
    );
    handle.shutdown().await.unwrap();
}

fn terminal_message_with_incomplete_replay(id: &str) -> AssistantMessage {
    let mut message = terminal_message(id, AssistantFinishReason::Stop);
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

#[tokio::test(flavor = "current_thread")]
async fn agent_handle_event_sinks_are_barriers() {
    // §10.9 Lifecycle. Pi basis: packages/agent/src/agent.ts processEvents
    // reduces state before listeners and awaits subscribers in registration
    // order before producer progress. Pi's stream result also permits a final
    // message without a preceding start event (agent-loop.ts lines 303-322).
    let runtime = TerminalOnlyRuntime::new([terminal_event(
        "terminal-finished",
        AssistantFinishReason::Stop,
    )]);
    let agent = Agent::new(Arc::new(runtime.clone()), state(), ToolRegistry::new()).unwrap();
    let handle = TokioAgentHandle::new(agent).unwrap();

    let first_entered = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let second_entered = Arc::new(Notify::new());
    let release_second = Arc::new(Notify::new());
    let second_started = Arc::new(AtomicBool::new(false));
    let assistant_started_snapshot = Arc::new(Mutex::new(None));

    handle
        .subscribe(sink({
            let first_entered = first_entered.clone();
            let release_first = release_first.clone();
            move |event, _cancellation| {
                let first_entered = first_entered.clone();
                let release_first = release_first.clone();
                Box::pin(async move {
                    if matches!(
                        event,
                        AgentEvent::MessageCommitted {
                            message: AgentRecord::Llm(Message::User(_))
                        }
                    ) {
                        first_entered.notify_one();
                        release_first.notified().await;
                    }
                })
            }
        }))
        .await
        .unwrap();
    handle
        .subscribe(sink({
            let handle = handle.clone();
            let assistant_started_snapshot = assistant_started_snapshot.clone();
            move |event, _cancellation| {
                let handle = handle.clone();
                let assistant_started_snapshot = assistant_started_snapshot.clone();
                Box::pin(async move {
                    if matches!(
                        event,
                        AgentEvent::MessageStarted {
                            role: MessageRole::Assistant,
                            ..
                        }
                    ) {
                        *lock(&assistant_started_snapshot) = handle.snapshot().streaming;
                    }
                })
            }
        }))
        .await
        .unwrap();
    handle
        .subscribe(sink({
            let second_entered = second_entered.clone();
            let release_second = release_second.clone();
            let second_started = second_started.clone();
            move |event, _cancellation| {
                let second_entered = second_entered.clone();
                let release_second = release_second.clone();
                let second_started = second_started.clone();
                Box::pin(async move {
                    if matches!(
                        event,
                        AgentEvent::MessageCommitted {
                            message: AgentRecord::Llm(Message::User(_))
                        }
                    ) {
                        second_started.store(true, Ordering::SeqCst);
                        second_entered.notify_one();
                        release_second.notified().await;
                    }
                })
            }
        }))
        .await
        .unwrap();

    let run = handle
        .prompt_records([user("user-1", "prompt")])
        .await
        .unwrap();
    first_entered.notified().await;
    tokio::task::yield_now().await;

    assert!(!second_started.load(Ordering::SeqCst));
    assert_eq!(
        runtime.remaining(),
        1,
        "provider request crossed first sink"
    );

    release_first.notify_one();
    second_entered.notified().await;
    assert_eq!(
        runtime.remaining(),
        1,
        "provider request crossed second sink"
    );

    release_second.notify_one();
    let outcome = run.outcome().await.unwrap();
    assert!(matches!(
        outcome,
        pi_agent_core::RunOutcome::Completed { .. }
    ));
    let started = lock(&assistant_started_snapshot)
        .clone()
        .expect("assistant MessageStarted sink must observe streaming state");
    assert_eq!(started.id, MessageId::new("terminal-finished"));
    assert!(started.terminal_message.is_none());
    handle.shutdown().await.unwrap();

    assert_terminal_only_through_handle(
        terminal_event("terminal-failed", AssistantFinishReason::Error),
        AssistantFinishReason::Error,
    )
    .await;
    assert_terminal_only_through_handle(
        terminal_event("terminal-cancelled", AssistantFinishReason::Aborted),
        AssistantFinishReason::Aborted,
    )
    .await;

    // Architecture v2 part 2 §1.3/R2 and §2.1: Pi-compatible terminal-only
    // streams still validate the event class, finish metadata, and successful
    // replay completeness before the actor observes a committed assistant.
    assert_malformed_terminal_only_through_handle(AssistantEvent::Failed {
        message: terminal_message("terminal-failed-with-success", AssistantFinishReason::Stop),
    })
    .await;
    for reason in [AssistantFinishReason::Error, AssistantFinishReason::Aborted] {
        assert_malformed_terminal_only_through_handle(AssistantEvent::Finished {
            message: terminal_message("terminal-finished-with-failure", reason),
        })
        .await;
    }
    assert_malformed_terminal_only_through_handle(AssistantEvent::Finished {
        message: terminal_message_with_incomplete_replay(
            "terminal-finished-with-incomplete-replay",
        ),
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn agent_wait_for_idle_includes_run_finished_sinks() {
    // §10.9 Lifecycle. Pi basis: packages/agent/src/agent.ts waitForIdle,
    // runWithLifecycle, processEvents, and finishRun.
    let runtime = ScriptedRuntime::new([text_response("done")]);
    let agent = Agent::new(Arc::new(runtime), state(), ToolRegistry::new()).unwrap();
    let handle = TokioAgentHandle::new(agent).unwrap();

    let run_finished_entered = Arc::new(Notify::new());
    let release_run_finished = Arc::new(Notify::new());
    handle
        .subscribe(sink({
            let run_finished_entered = run_finished_entered.clone();
            let release_run_finished = release_run_finished.clone();
            move |event, _cancellation| {
                let run_finished_entered = run_finished_entered.clone();
                let release_run_finished = release_run_finished.clone();
                Box::pin(async move {
                    if matches!(event, AgentEvent::RunFinished { .. }) {
                        run_finished_entered.notify_one();
                        release_run_finished.notified().await;
                    }
                })
            }
        }))
        .await
        .unwrap();

    let run = handle
        .prompt_records([user("user-1", "prompt")])
        .await
        .unwrap();
    let idle_handle = handle.clone();
    let idle = tokio::spawn(async move { idle_handle.wait_for_idle().await });

    run_finished_entered.notified().await;
    tokio::task::yield_now().await;
    assert!(!idle.is_finished(), "idle resolved before RunFinished sink");

    release_run_finished.notify_one();
    let outcome = run.outcome().await.unwrap();
    assert!(matches!(
        outcome,
        pi_agent_core::RunOutcome::Completed { .. }
    ));
    idle.await.unwrap().unwrap();
    handle.shutdown().await.unwrap();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
