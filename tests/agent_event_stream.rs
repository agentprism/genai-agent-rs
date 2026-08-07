use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::FusedStream;
use futures::{Stream, StreamExt};
use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use rust_genai_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AssistantContent, AssistantMessage,
    AssistantMessageEventStream, CustomMessage, LoopError, StopReason, StreamFn, StreamRequest,
    agent_loop, agent_loop_continue, default_convert_to_llm, set_default_stream_fn,
};
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn model_iden() -> ModelIden {
    ModelIden::new(AdapterKind::OpenAIResp, "event-stream-test")
}

fn config() -> AgentLoopConfig {
    AgentLoopConfig::new(ModelSpec::from_iden(model_iden()), default_convert_to_llm())
}

fn response(text: impl Into<String>) -> AssistantMessage {
    AssistantMessage::completed(
        model_iden(),
        vec![AssistantContent::text(text)],
        StopReason::Stop,
    )
}

#[derive(Clone)]
struct MessageStreamFn {
    message: AssistantMessage,
}

#[async_trait]
impl StreamFn for MessageStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        AssistantMessageEventStream::from_message(self.message.clone())
    }
}

struct MalformedStreamFn;

#[async_trait]
impl StreamFn for MalformedStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        AssistantMessageEventStream::from_events(Vec::new())
    }
}

struct PanickingStreamFn;

#[async_trait]
impl StreamFn for PanickingStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        panic!("provider boundary exploded")
    }
}

struct PendingStreamFn;

#[async_trait]
impl StreamFn for PendingStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        AssistantMessageEventStream::from_stream(futures::stream::pending())
    }
}

struct PendingDropStream {
    dropped: Option<oneshot::Sender<()>>,
}

impl Stream for PendingDropStream {
    type Item = rust_genai_agent::AssistantMessageEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for PendingDropStream {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

struct DropObservedStreamFn {
    started: Mutex<Option<oneshot::Sender<()>>>,
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl StreamFn for DropObservedStreamFn {
    async fn stream(&self, _request: StreamRequest) -> AssistantMessageEventStream {
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        let dropped = self.dropped.lock().unwrap().take();
        AssistantMessageEventStream::from_stream(PendingDropStream { dropped })
    }
}

#[tokio::test]
async fn event_iteration_and_cloneable_result_handles_are_equivalent_and_fused() {
    let prompt = AgentMessage::user("hello");
    let assistant = response("hi");
    let provider = Arc::new(MessageStreamFn {
        message: assistant.clone(),
    });
    let mut stream = agent_loop(
        vec![prompt.clone()],
        AgentContext::new("be helpful"),
        config(),
        CancellationToken::new(),
        Some(provider),
    );
    let first_result = stream.result_handle();
    let second_result = first_result.clone();

    let events = stream.by_ref().collect::<Vec<_>>().await;
    assert!(stream.is_terminated());
    assert_eq!(stream.next().await, None);
    assert_eq!(stream.next().await, None);

    let messages = first_result.get().await.unwrap();
    assert_eq!(second_result.get().await.unwrap(), messages);
    assert_eq!(messages, vec![prompt, AgentMessage::Assistant(assistant)]);

    let event_messages = events.iter().find_map(|event| match event {
        AgentEvent::AgentEnd { messages } => Some(messages),
        _ => None,
    });
    assert_eq!(event_messages, Some(&messages));
}

#[tokio::test]
async fn result_resolves_without_polling_and_events_remain_available() {
    let provider = Arc::new(MessageStreamFn {
        message: response("unobserved events do not apply backpressure"),
    });
    let mut stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        Some(provider),
    );
    let result = stream.result_handle();

    let messages = tokio::time::timeout(Duration::from_secs(1), result.get())
        .await
        .expect("result hung while the event stream was not polled")
        .unwrap();
    assert_eq!(messages.len(), 2);

    let events = stream.by_ref().collect::<Vec<_>>().await;
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
    assert!(stream.is_terminated());
}

#[test]
fn continue_guards_are_synchronous_and_compare_role_strings() {
    let empty = agent_loop_continue(
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        None,
    );
    assert!(matches!(empty, Err(LoopError::EmptyContext)));

    let custom_assistant = AgentMessage::Custom(CustomMessage::new(
        "assistant",
        json!({ "application_owned": true }),
    ));
    let custom = agent_loop_continue(
        AgentContext::default().with_messages(vec![custom_assistant]),
        config(),
        CancellationToken::new(),
        None,
    );
    assert!(matches!(custom, Err(LoopError::ContinueFromAssistant)));

    let ordinary_assistant = AgentMessage::Assistant(response("already complete"));
    let ordinary = agent_loop_continue(
        AgentContext::default().with_messages(vec![ordinary_assistant]),
        config(),
        CancellationToken::new(),
        None,
    );
    assert!(matches!(ordinary, Err(LoopError::ContinueFromAssistant)));
}

#[tokio::test]
async fn continue_accepts_a_non_assistant_custom_role() {
    let provider = Arc::new(MessageStreamFn {
        message: response("continued"),
    });
    let context = AgentContext::default().with_messages(vec![AgentMessage::Custom(
        CustomMessage::new("application", json!({ "resume": true })),
    )]);

    let stream =
        agent_loop_continue(context, config(), CancellationToken::new(), Some(provider)).unwrap();
    let messages = stream.result().await.unwrap();

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role(), "assistant");
}

#[tokio::test]
async fn dropping_event_iteration_does_not_prevent_a_retained_result() {
    let cancel = CancellationToken::new();
    let stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        cancel.clone(),
        Some(Arc::new(PendingStreamFn)),
    );
    let result = stream.result_handle();
    drop(stream);
    cancel.cancel();

    let messages = tokio::time::timeout(Duration::from_secs(1), result.get())
        .await
        .expect("dropping event iteration stalled the loop task")
        .unwrap();
    assert_eq!(messages.len(), 2);
    let AgentMessage::Assistant(message) = messages.last().unwrap() else {
        panic!("expected a terminal assistant message")
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn dropping_all_stream_owners_aborts_an_unobservable_background_task() {
    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let provider = Arc::new(DropObservedStreamFn {
        started: Mutex::new(Some(started_tx)),
        dropped: Mutex::new(Some(dropped_tx)),
    });
    let stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        Some(provider),
    );

    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("provider boundary was not entered")
        .unwrap();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("unobservable loop task leaked after its stream was dropped")
        .unwrap();
}

#[tokio::test]
async fn cancellation_token_is_forwarded_to_the_spawned_loop() {
    let cancel = CancellationToken::new();
    let stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        cancel.clone(),
        Some(Arc::new(PendingStreamFn)),
    );
    let result = stream.result_handle();
    cancel.cancel();

    let messages = tokio::time::timeout(Duration::from_secs(1), result.get())
        .await
        .expect("cancelled loop did not settle")
        .unwrap();
    let AgentMessage::Assistant(message) = messages.last().unwrap() else {
        panic!("expected a terminal assistant message")
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn a_process_default_stream_fn_is_resolved_inside_the_spawned_loop() {
    let provider = Arc::new(MessageStreamFn {
        message: response("process default"),
    });
    let previous = set_default_stream_fn(Some(provider));
    let stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        None,
    );
    let outcome = stream.result().await;
    set_default_stream_fn(previous);

    assert_eq!(outcome.unwrap().len(), 2);
}

#[tokio::test]
async fn a_malformed_assistant_stream_still_settles_the_wrapper_result() {
    let stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        Some(Arc::new(MalformedStreamFn)),
    );

    let messages = tokio::time::timeout(Duration::from_secs(1), stream.result())
        .await
        .expect("malformed boundary left the wrapper result pending")
        .unwrap();
    let AgentMessage::Assistant(message) = messages.last().unwrap() else {
        panic!("expected a synthesized assistant error")
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("terminal Done or Error"))
    );
}

#[tokio::test]
async fn a_panicking_boundary_resolves_an_error_and_closes_a_fused_stream() {
    let mut stream = agent_loop(
        vec![AgentMessage::user("hello")],
        AgentContext::default(),
        config(),
        CancellationToken::new(),
        Some(Arc::new(PanickingStreamFn)),
    );
    let result = stream.result_handle();

    let error = tokio::time::timeout(Duration::from_secs(1), result.get())
        .await
        .expect("panicking loop task left the result pending")
        .unwrap_err();
    assert!(matches!(
        error,
        LoopError::TaskPanicked(message) if message.contains("provider boundary exploded")
    ));

    while stream.next().await.is_some() {}
    assert!(stream.is_terminated());
    assert_eq!(stream.next().await, None);
    assert_eq!(stream.next().await, None);
}
