//! Assistant events, partial snapshots, and terminal settlement.

use crate::types::{AssistantMessage, ErrorStopReason, JsString, SuccessfulStopReason, ToolCall};
use futures::Stream;
use futures::stream::FusedStream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::sync::{mpsc, watch};

/// pi `types.ts:536-545`: every nonterminal event carries the current assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantMessageEvent {
    Start {
        partial: Arc<AssistantMessage>,
    },
    TextStart {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        partial: Arc<AssistantMessage>,
    },
    TextDelta {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        delta: JsString,
        partial: Arc<AssistantMessage>,
    },
    TextEnd {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        content: JsString,
        partial: Arc<AssistantMessage>,
    },
    ThinkingStart {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        partial: Arc<AssistantMessage>,
    },
    ThinkingDelta {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        delta: JsString,
        partial: Arc<AssistantMessage>,
    },
    ThinkingEnd {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        content: JsString,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        delta: JsString,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(serialize_with = "crate::types::serialize_js_f64")]
        content_index: f64,
        tool_call: ToolCall,
        partial: Arc<AssistantMessage>,
    },
    Done {
        reason: SuccessfulStopReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorStopReason,
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    pub fn partial(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Start { partial }
            | Self::TextStart { partial, .. }
            | Self::TextDelta { partial, .. }
            | Self::TextEnd { partial, .. }
            | Self::ThinkingStart { partial, .. }
            | Self::ThinkingDelta { partial, .. }
            | Self::ThinkingEnd { partial, .. }
            | Self::ToolCallStart { partial, .. }
            | Self::ToolCallDelta { partial, .. }
            | Self::ToolCallEnd { partial, .. } => Some(partial),
            Self::Done { .. } | Self::Error { .. } => None,
        }
    }

    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum MessageBuilderError {
    #[error("content index {index} is not a non-negative safe integer")]
    InvalidContentIndex { index: f64 },
    #[error("content index {index} is not contiguous; next content index is {next}")]
    NonContiguousContentIndex { index: usize, next: usize },
    #[error("received {event} for non-{expected} content at index {index}")]
    ContentTypeMismatch {
        event: &'static str,
        expected: &'static str,
        index: usize,
    },
    #[error("message builder received an event after terminal settlement")]
    EventAfterTerminal,
}

/// Reconstructs the snapshots that pi carries on every nonterminal event.
#[derive(Debug, Clone)]
pub struct MessageBuilder {
    message: AssistantMessage,
    terminal: bool,
}

impl MessageBuilder {
    pub fn new(message: AssistantMessage) -> Self {
        Self {
            message,
            terminal: false,
        }
    }

    pub fn snapshot(&self) -> &AssistantMessage {
        &self.message
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub fn apply(
        &mut self,
        event: &AssistantMessageEvent,
    ) -> Result<&AssistantMessage, MessageBuilderError> {
        if self.terminal {
            return Err(MessageBuilderError::EventAfterTerminal);
        }

        match event {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                self.message.clone_from(partial);
            }
            AssistantMessageEvent::Done { message, .. } => {
                self.message.clone_from(message);
                self.terminal = true;
            }
            AssistantMessageEvent::Error { error, .. } => {
                self.message.clone_from(error);
                self.terminal = true;
            }
        }
        Ok(&self.message)
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StreamProtocolError {
    #[error("assistant event stream closed without a terminal done or error event")]
    MissingTerminalEvent,
}

#[derive(Debug, Clone)]
enum StreamResultState {
    Pending,
    Terminal(Box<AssistantMessage>),
}

#[derive(Clone)]
pub struct AssistantMessageResult {
    receiver: watch::Receiver<StreamResultState>,
}

impl AssistantMessageResult {
    pub async fn get(mut self) -> Result<AssistantMessage, StreamProtocolError> {
        loop {
            match self.receiver.borrow().clone() {
                StreamResultState::Pending => {}
                StreamResultState::Terminal(message) => return Ok(*message),
            }
            if self.receiver.changed().await.is_err() {
                futures::future::pending::<()>().await;
            }
        }
    }
}

fn publish_first_terminal(sender: &watch::Sender<StreamResultState>, message: AssistantMessage) {
    sender.send_if_modified(|state| {
        if !matches!(state, StreamResultState::Pending) {
            return false;
        }
        *state = StreamResultState::Terminal(Box::new(message));
        true
    });
}

/// A single-consumer event stream with an independently cloneable terminal-result handle.
pub struct AssistantMessageEventStream {
    inner: Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + 'static>>,
    terminal_sender: Option<watch::Sender<StreamResultState>>,
    result: AssistantMessageResult,
    terminated: bool,
}

impl std::fmt::Debug for AssistantMessageEventStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssistantMessageEventStream")
            .field("terminated", &self.terminated)
            .finish_non_exhaustive()
    }
}

impl AssistantMessageEventStream {
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = AssistantMessageEvent> + Send + 'static,
    {
        let (terminal_sender, receiver) = watch::channel(StreamResultState::Pending);
        Self {
            inner: Box::pin(stream),
            terminal_sender: Some(terminal_sender),
            result: AssistantMessageResult { receiver },
            terminated: false,
        }
    }

    pub fn from_events(events: Vec<AssistantMessageEvent>) -> Self {
        let (sender, stream) = Self::channel();
        for event in events {
            if sender.send(event).is_err() {
                break;
            }
        }
        drop(sender);
        stream
    }

    pub fn channel() -> (AssistantStreamSender, Self) {
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let (terminal_sender, receiver) = watch::channel(StreamResultState::Pending);
        let producer_terminal = terminal_sender.clone();
        let producer_state = Arc::new(Mutex::new(ProducerState {
            completed: false,
            senders: 1,
        }));
        let stream = futures::stream::poll_fn(move |context| event_receiver.poll_recv(context));
        (
            AssistantStreamSender {
                event_sender,
                terminal_sender: producer_terminal,
                producer_state,
            },
            Self {
                inner: Box::pin(stream),
                terminal_sender: Some(terminal_sender),
                result: AssistantMessageResult { receiver },
                terminated: false,
            },
        )
    }

    pub fn result_handle(&self) -> AssistantMessageResult {
        self.result.clone()
    }

    pub fn result(
        &self,
    ) -> impl Future<Output = Result<AssistantMessage, StreamProtocolError>> + Send + 'static {
        let result = self.result.clone();
        async move { result.get().await }
    }
}

impl Drop for AssistantMessageEventStream {
    fn drop(&mut self) {
        self.terminal_sender.take();
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                if let Some(message) = event.terminal_message() {
                    if let Some(sender) = self.terminal_sender.take() {
                        publish_first_terminal(&sender, message.clone());
                    }
                    self.terminated = true;
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                self.terminal_sender.take();
                self.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedStream for AssistantMessageEventStream {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
#[error("assistant event stream receiver is closed")]
pub struct AssistantStreamSendError;

pub struct AssistantStreamSender {
    event_sender: mpsc::UnboundedSender<AssistantMessageEvent>,
    terminal_sender: watch::Sender<StreamResultState>,
    producer_state: Arc<Mutex<ProducerState>>,
}

struct ProducerState {
    completed: bool,
    senders: usize,
}

impl Clone for AssistantStreamSender {
    fn clone(&self) -> Self {
        self.producer_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .senders += 1;
        Self {
            event_sender: self.event_sender.clone(),
            terminal_sender: self.terminal_sender.clone(),
            producer_state: self.producer_state.clone(),
        }
    }
}

impl AssistantStreamSender {
    /// The first accepted terminal event wins across every sender clone; later sends are ignored.
    pub fn send(&self, event: AssistantMessageEvent) -> Result<(), AssistantStreamSendError> {
        let mut state = self
            .producer_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if state.completed {
            return Ok(());
        }

        let terminal = event.terminal_message().cloned();
        self.event_sender
            .send(event)
            .map_err(|_| AssistantStreamSendError)?;
        if let Some(message) = terminal {
            state.completed = true;
            publish_first_terminal(&self.terminal_sender, message);
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.producer_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .completed
            || self.event_sender.is_closed()
    }
}

impl Drop for AssistantStreamSender {
    fn drop(&mut self) {
        let mut state = self
            .producer_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.senders -= 1;
        if state.senders == 0 && !state.completed {
            state.completed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Api, AssistantContent, AssistantRole, ProviderId, StopReason, TextContent, Usage,
    };
    use futures::StreamExt;
    use serde_json::json;
    use tokio::sync::Barrier;

    fn message(reason: StopReason, label: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending("test-api", "test-provider", "test-model", 1.0);
        message.stop_reason = reason;
        message.content = vec![AssistantContent::Text(TextContent::new(label))];
        message
    }

    fn partial() -> Arc<AssistantMessage> {
        Arc::new(message(StopReason::Pending, "partial"))
    }

    /// Pins pi `types.ts:528-552` and `utils/event-stream.ts:19-38`: terminals own `result()`.
    #[tokio::test]
    async fn terminal_event_is_authoritative_result() {
        let expected = message(StopReason::Stop, "authoritative");
        let mut stream = AssistantMessageEventStream::from_events(vec![
            AssistantMessageEvent::Start { partial: partial() },
            AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::Stop,
                message: expected.clone(),
            },
        ]);
        let result = stream.result();
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(result.await.unwrap(), expected);
    }

    /// Pins pi `utils/event-stream.ts:25-37,47-55`: the first clone-raced terminal settles once.
    #[tokio::test]
    async fn channel_terminal_settles_exactly_once_across_sender_clones() {
        let (sender, mut stream) = AssistantMessageEventStream::channel();
        let second = sender.clone();
        let barrier = Arc::new(Barrier::new(3));
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let left = tokio::spawn(async move {
            left_barrier.wait().await;
            sender
                .send(AssistantMessageEvent::Done {
                    reason: SuccessfulStopReason::Stop,
                    message: message(StopReason::Stop, "left"),
                })
                .unwrap();
        });
        let right = tokio::spawn(async move {
            right_barrier.wait().await;
            second
                .send(AssistantMessageEvent::Error {
                    reason: ErrorStopReason::Error,
                    error: message(StopReason::Error, "right"),
                })
                .unwrap();
        });
        barrier.wait().await;
        left.await.unwrap();
        right.await.unwrap();

        let result = stream.result().await.unwrap();
        let events = stream.by_ref().collect::<Vec<_>>().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].terminal_message(), Some(&result));
        assert!(stream.next().await.is_none());
    }

    /// Pins pi `utils/event-stream.ts:22-23,42-64`: terminal delivery fuses the iterator.
    #[tokio::test]
    async fn stream_is_fused_after_first_terminal() {
        let terminal = message(StopReason::Stop, "done");
        let mut stream = AssistantMessageEventStream::from_events(vec![
            AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::Stop,
                message: terminal,
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0.0,
                delta: "late".into(),
                partial: partial(),
            },
        ]);
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert!(stream.is_terminated());
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.next().await, None);
    }

    /// Ports pi `utils/event-stream.ts:47-55`: close without a terminal leaves result pending.
    #[tokio::test]
    async fn close_without_terminal_leaves_result_pending() {
        let mut stream =
            AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Start {
                partial: partial(),
            }]);
        let result = stream.result_handle();
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert_eq!(stream.next().await, None);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), result.get())
                .await
                .is_err()
        );
    }

    /// Ports pi `utils/event-stream.ts:47-55` for an unconsumed channel whose producers disappear.
    #[tokio::test]
    async fn last_channel_producer_drop_leaves_result_pending_without_iteration() {
        let (sender, stream) = AssistantMessageEventStream::channel();
        let second = sender.clone();
        let result = stream.result();
        sender
            .send(AssistantMessageEvent::Start { partial: partial() })
            .unwrap();
        let left = tokio::spawn(async move { drop(sender) });
        let right = tokio::spawn(async move { drop(second) });
        left.await.unwrap();
        right.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), result)
                .await
                .is_err()
        );
    }

    /// Pins pi `utils/event-stream.ts:31-36,47-64`: channel events preserve order and settle unpolled results.
    #[tokio::test]
    async fn channel_result_does_not_require_iteration_and_events_remain_ordered() {
        let (sender, mut stream) = AssistantMessageEventStream::channel();
        let expected = message(StopReason::Length, "limit");
        sender
            .send(AssistantMessageEvent::Start { partial: partial() })
            .unwrap();
        sender
            .send(AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::Length,
                message: expected.clone(),
            })
            .unwrap();
        assert!(sender.is_closed());
        assert_eq!(stream.result().await.unwrap(), expected);
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(stream.next().await, None);
    }

    /// Pins pi `types.ts:547-552`: terminals replace the accumulated snapshot.
    #[test]
    fn message_builder_applies_done_and_error_terminals_authoritatively() {
        let base = AssistantMessage::pending("api", "provider", "model", 7.0);
        let mut done_builder = MessageBuilder::new(base.clone());
        let done = message(StopReason::ToolUse, "final");
        done_builder
            .apply(&AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::ToolUse,
                message: done.clone(),
            })
            .unwrap();
        assert_eq!(done_builder.snapshot(), &done);
        assert!(done_builder.is_terminal());

        let mut error_builder = MessageBuilder::new(base);
        error_builder
            .apply(&AssistantMessageEvent::TextStart {
                content_index: 0.0,
                partial: partial(),
            })
            .unwrap();
        error_builder
            .apply(&AssistantMessageEvent::TextDelta {
                content_index: 0.0,
                delta: "partial".into(),
                partial: partial(),
            })
            .unwrap();
        let mut error = AssistantMessage {
            role: AssistantRole::Assistant,
            content: error_builder.snapshot().content.clone(),
            api: Api::from("api"),
            provider: ProviderId::from("provider"),
            model: "model".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Aborted,
            deferred: None,
            error_message: Some("Request aborted by user".into()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 7.0,
        };
        error_builder
            .apply(&AssistantMessageEvent::Error {
                reason: ErrorStopReason::Aborted,
                error: error.clone(),
            })
            .unwrap();
        assert_eq!(error_builder.snapshot(), &error);
        error.error_message = Some("changed after clone".into());
        assert_ne!(error_builder.snapshot(), &error);
    }

    /// Pins pi `types.ts:536-552`: every nonterminal event serializes its
    /// current assistant-message snapshot under `partial`.
    #[test]
    fn event_wire_carries_pi_partial_snapshots() {
        let snapshot = message(StopReason::Pending, "snapshot");
        let partial = || Arc::new(snapshot.clone());
        let events = vec![
            AssistantMessageEvent::Start { partial: partial() },
            AssistantMessageEvent::TextStart {
                content_index: 0.0,
                partial: partial(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0.0,
                delta: "a".into(),
                partial: partial(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0.0,
                content: "a".into(),
                partial: partial(),
            },
            AssistantMessageEvent::ThinkingStart {
                content_index: 1.0,
                partial: partial(),
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 1.0,
                delta: "b".into(),
                partial: partial(),
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 1.0,
                content: "b".into(),
                partial: partial(),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 2.0,
                partial: partial(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2.0,
                delta: "{}".into(),
                partial: partial(),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 2.0,
                tool_call: ToolCall::new("call", "lookup", crate::types::JsonObject::new()),
                partial: partial(),
            },
        ];
        let wire = serde_json::to_value(&events).unwrap();
        let wire = wire.as_array().expect("event array");
        assert_eq!(wire.len(), 10);
        for (event, wire_event) in events.iter().zip(wire) {
            assert_eq!(event.partial(), Some(&snapshot));
            assert_eq!(wire_event.get("partial"), Some(&json!(snapshot)));
        }
        assert_eq!(
            wire[4],
            json!({"type":"thinking_start","contentIndex":1,"partial":snapshot})
        );
        assert_eq!(
            wire[5],
            json!({"type":"thinking_delta","contentIndex":1,"delta":"b","partial":snapshot})
        );
        assert_eq!(
            wire[6],
            json!({"type":"thinking_end","contentIndex":1,"content":"b","partial":snapshot})
        );
        assert_eq!(
            wire[7],
            json!({"type":"toolcall_start","contentIndex":2,"partial":snapshot})
        );

        let done_message = message(StopReason::Deferred, "later");
        let done_wire = serde_json::to_value(AssistantMessageEvent::Done {
            reason: SuccessfulStopReason::Deferred,
            message: done_message.clone(),
        })
        .unwrap();
        assert_eq!(
            done_wire,
            json!({"type":"done","reason":"deferred","message":done_message})
        );

        let mut error_message = message(StopReason::Error, "partial");
        error_message.error_message = Some("failed".into());
        let error_wire = serde_json::to_value(AssistantMessageEvent::Error {
            reason: ErrorStopReason::Error,
            error: error_message.clone(),
        })
        .unwrap();
        assert_eq!(
            error_wire,
            json!({"type":"error","reason":"error","error":error_message})
        );
    }
}
