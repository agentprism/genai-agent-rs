//! Partial-free assistant events, terminal settlement, and snapshot reconstruction.

use crate::types::{
    AssistantContent, AssistantMessage, ErrorStopReason, SuccessfulStopReason, TextContent,
    ThinkingContent, ToolCall,
};
use crate::utils::json_parse::parse_streaming_json;
use futures::Stream;
use futures::stream::FusedStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::sync::{mpsc, watch};

/// The canonical Rust event wire omits pi's shared-reference `partial` snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantMessageEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
    },
    ThinkingStart {
        content_index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature_delta: Option<String>,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        id: String,
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
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
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MessageBuilderError {
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
    raw_tool_arguments: BTreeMap<usize, String>,
    terminal: bool,
}

impl MessageBuilder {
    pub fn new(message: AssistantMessage) -> Self {
        Self {
            message,
            raw_tool_arguments: BTreeMap::new(),
            terminal: false,
        }
    }

    pub fn snapshot(&self) -> &AssistantMessage {
        &self.message
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Applies the proxy reconstruction rules from pi `packages/agent/src/proxy.ts:240-362`.
    pub fn apply(
        &mut self,
        event: &AssistantMessageEvent,
    ) -> Result<&AssistantMessage, MessageBuilderError> {
        if self.terminal {
            return Err(MessageBuilderError::EventAfterTerminal);
        }

        match event {
            AssistantMessageEvent::Start => {}
            AssistantMessageEvent::TextStart { content_index } => {
                self.set_content(*content_index, AssistantContent::Text(TextContent::new("")))?;
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::Text(content)) => content.text.push_str(delta),
                _ => {
                    return Err(Self::type_mismatch("text_delta", "text", *content_index));
                }
            },
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                content_signature,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::Text(block)) => {
                    block.text.clone_from(content);
                    block.text_signature.clone_from(content_signature);
                }
                _ => {
                    return Err(Self::type_mismatch("text_end", "text", *content_index));
                }
            },
            AssistantMessageEvent::ThinkingStart {
                content_index,
                thinking,
                thinking_signature,
                redacted,
            } => {
                let mut block = ThinkingContent::new(thinking.as_deref().unwrap_or_default());
                block.thinking_signature.clone_from(thinking_signature);
                block.redacted = *redacted;
                self.set_content(*content_index, AssistantContent::Thinking(block))?;
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                thinking_signature_delta,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::Thinking(content)) => {
                    content.thinking.push_str(delta);
                    if let Some(signature_delta) = thinking_signature_delta {
                        content
                            .thinking_signature
                            .get_or_insert_with(String::new)
                            .push_str(signature_delta);
                    }
                }
                _ => {
                    return Err(Self::type_mismatch(
                        "thinking_delta",
                        "thinking",
                        *content_index,
                    ));
                }
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                content_signature,
                redacted,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::Thinking(block)) => {
                    block.thinking.clone_from(content);
                    if let Some(content_signature) = content_signature {
                        block.thinking_signature = Some(content_signature.clone());
                    }
                    if redacted.is_some() {
                        block.redacted = *redacted;
                    }
                }
                _ => {
                    return Err(Self::type_mismatch(
                        "thinking_end",
                        "thinking",
                        *content_index,
                    ));
                }
            },
            AssistantMessageEvent::ToolCallStart {
                content_index,
                id,
                tool_name,
                namespace,
            } => {
                let mut tool_call = ToolCall::new(id, tool_name, Value::Object(Default::default()));
                tool_call.namespace.clone_from(namespace);
                self.set_content(*content_index, AssistantContent::ToolCall(tool_call))?;
                self.raw_tool_arguments
                    .insert(*content_index, String::new());
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::ToolCall(call)) => {
                    let raw = self.raw_tool_arguments.entry(*content_index).or_default();
                    raw.push_str(delta);
                    call.arguments = parse_proxy_streaming_arguments(raw);
                }
                _ => {
                    return Err(Self::type_mismatch(
                        "toolcall_delta",
                        "toolCall",
                        *content_index,
                    ));
                }
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
            } => {
                // pi's proxy ignores a tool-call end when its start block was not retained.
                if matches!(
                    self.message.content.get(*content_index),
                    Some(AssistantContent::ToolCall(_))
                ) {
                    self.message.content[*content_index] =
                        AssistantContent::ToolCall(tool_call.clone());
                    self.raw_tool_arguments.remove(content_index);
                }
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

    fn set_content(
        &mut self,
        index: usize,
        content: AssistantContent,
    ) -> Result<(), MessageBuilderError> {
        match index.cmp(&self.message.content.len()) {
            std::cmp::Ordering::Less => self.message.content[index] = content,
            std::cmp::Ordering::Equal => self.message.content.push(content),
            std::cmp::Ordering::Greater => {
                return Err(MessageBuilderError::NonContiguousContentIndex {
                    index,
                    next: self.message.content.len(),
                });
            }
        }
        Ok(())
    }

    fn type_mismatch(
        event: &'static str,
        expected: &'static str,
        index: usize,
    ) -> MessageBuilderError {
        MessageBuilderError::ContentTypeMismatch {
            event,
            expected,
            index,
        }
    }
}

fn parse_proxy_streaming_arguments(raw: &str) -> Value {
    match parse_streaming_json(Some(raw)) {
        Value::Null | Value::Bool(false) => Value::Object(Default::default()),
        Value::Number(number) if number.as_f64() == Some(0.0) => Value::Object(Default::default()),
        Value::String(value) if value.is_empty() => Value::Object(Default::default()),
        value => value,
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
    MissingTerminal,
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
                StreamResultState::MissingTerminal => {
                    return Err(StreamProtocolError::MissingTerminalEvent);
                }
            }
            self.receiver
                .changed()
                .await
                .map_err(|_| StreamProtocolError::MissingTerminalEvent)?;
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

fn publish_missing_terminal(sender: &watch::Sender<StreamResultState>) {
    sender.send_if_modified(|state| {
        if !matches!(state, StreamResultState::Pending) {
            return false;
        }
        *state = StreamResultState::MissingTerminal;
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
        if let Some(sender) = self.terminal_sender.take() {
            publish_missing_terminal(&sender);
        }
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
                if let Some(sender) = self.terminal_sender.take() {
                    publish_missing_terminal(&sender);
                }
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
            publish_missing_terminal(&self.terminal_sender);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, AssistantRole, ProviderId, StopReason, Usage};
    use futures::StreamExt;
    use serde_json::json;
    use tokio::sync::Barrier;

    fn message(reason: StopReason, label: &str) -> AssistantMessage {
        let mut message = AssistantMessage::pending("test-api", "test-provider", "test-model", 1);
        message.stop_reason = reason;
        message.content = vec![AssistantContent::Text(TextContent::new(label))];
        message
    }

    /// Pins pi `types.ts:528-552` and `utils/event-stream.ts:19-38`: terminals own `result()`.
    #[tokio::test]
    async fn terminal_event_is_authoritative_result() {
        let expected = message(StopReason::Stop, "authoritative");
        let mut stream = AssistantMessageEventStream::from_events(vec![
            AssistantMessageEvent::Start,
            AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::Stop,
                message: expected.clone(),
            },
        ]);
        let result = stream.result();
        assert_eq!(stream.next().await, Some(AssistantMessageEvent::Start));
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
                content_index: 0,
                delta: "late".into(),
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

    /// Pins the seam-#5 terminal requirement over pi `utils/event-stream.ts:47-55`'s unresolved case.
    #[tokio::test]
    async fn close_without_terminal_is_a_protocol_error() {
        let mut stream =
            AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Start]);
        let result = stream.result_handle();
        assert_eq!(stream.next().await, Some(AssistantMessageEvent::Start));
        assert_eq!(stream.next().await, None);
        assert_eq!(
            result.get().await,
            Err(StreamProtocolError::MissingTerminalEvent)
        );
    }

    /// Pins the seam-#5 terminal requirement for an unconsumed channel whose producers disappear.
    #[tokio::test]
    async fn last_channel_producer_drop_reports_missing_terminal_without_iteration() {
        let (sender, stream) = AssistantMessageEventStream::channel();
        let second = sender.clone();
        let result = stream.result();
        sender.send(AssistantMessageEvent::Start).unwrap();
        let left = tokio::spawn(async move { drop(sender) });
        let right = tokio::spawn(async move { drop(second) });
        left.await.unwrap();
        right.await.unwrap();
        assert_eq!(result.await, Err(StreamProtocolError::MissingTerminalEvent));
    }

    /// Pins pi `utils/event-stream.ts:31-36,47-64`: channel events preserve order and settle unpolled results.
    #[tokio::test]
    async fn channel_result_does_not_require_iteration_and_events_remain_ordered() {
        let (sender, mut stream) = AssistantMessageEventStream::channel();
        let expected = message(StopReason::Length, "limit");
        sender.send(AssistantMessageEvent::Start).unwrap();
        sender
            .send(AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::Length,
                message: expected.clone(),
            })
            .unwrap();
        assert!(sender.is_closed());
        assert_eq!(stream.result().await.unwrap(), expected);
        assert_eq!(stream.next().await, Some(AssistantMessageEvent::Start));
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(stream.next().await, None);
    }

    /// Pins pi `proxy.ts:240-356`: text, thinking, and parallel streamed calls rebuild by index.
    #[test]
    fn message_builder_reconstructs_interleaved_content_and_partial_arguments() {
        let base = AssistantMessage::pending("api", "provider", "model", 7);
        let mut builder = MessageBuilder::new(base);
        let first_arguments = json!({"city":"Paris"});
        let second_arguments = json!({"days":2});
        let events = vec![
            AssistantMessageEvent::Start,
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                thinking: None,
                thinking_signature: None,
                redacted: None,
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "plan".into(),
                thinking_signature_delta: None,
            },
            AssistantMessageEvent::TextStart { content_index: 1 },
            AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "Calling tools".into(),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 2,
                id: "a".into(),
                tool_name: "weather".into(),
                namespace: None,
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 3,
                id: "b".into(),
                tool_name: "calendar".into(),
                namespace: None,
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2,
                delta: "{\"city\":\"Par".into(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 3,
                delta: "{\"days\":".into(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2,
                delta: "is\"}".into(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 3,
                delta: "2}".into(),
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "plan".into(),
                content_signature: Some("thinking-sig".into()),
                redacted: Some(false),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 1,
                content: "Calling tools".into(),
                content_signature: Some("text-sig".into()),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 2,
                tool_call: ToolCall {
                    thought_signature: Some("tool-sig".into()),
                    ..ToolCall::new("a", "weather", first_arguments.clone())
                },
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 3,
                tool_call: ToolCall::new("b", "calendar", second_arguments.clone()),
            },
        ];

        for event in &events {
            builder.apply(event).unwrap();
        }
        assert_eq!(builder.snapshot().content.len(), 4);
        assert!(matches!(
            &builder.snapshot().content[0],
            AssistantContent::Thinking(content)
                if content.thinking == "plan"
                    && content.thinking_signature.as_deref() == Some("thinking-sig")
        ));
        assert!(matches!(
            &builder.snapshot().content[1],
            AssistantContent::Text(content)
                if content.text == "Calling tools"
                    && content.text_signature.as_deref() == Some("text-sig")
        ));
        assert!(matches!(
            &builder.snapshot().content[2],
            AssistantContent::ToolCall(call) if call.arguments == first_arguments
        ));
        assert!(matches!(
            &builder.snapshot().content[3],
            AssistantContent::ToolCall(call) if call.arguments == second_arguments
        ));
    }

    /// Pins pi `src/utils/json-parse.ts:104-123` and `packages/agent/src/proxy.ts:322-327`.
    #[test]
    fn message_builder_uses_pi_streaming_json_values_without_a_depth_cap() {
        fn arguments(builder: &MessageBuilder) -> &Value {
            let AssistantContent::ToolCall(call) = &builder.snapshot().content[0] else {
                panic!("tool call")
            };
            &call.arguments
        }

        let mut builder =
            MessageBuilder::new(AssistantMessage::pending("api", "provider", "model", 1));
        builder
            .apply(&AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id: "call".into(),
                tool_name: "run".into(),
                namespace: None,
            })
            .unwrap();
        builder
            .apply(&AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":\"A\\".into(),
            })
            .unwrap();
        assert_eq!(arguments(&builder), &json!({"path":"A"}));
        builder
            .apply(&AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "H\"}".into(),
            })
            .unwrap();
        assert_eq!(arguments(&builder), &json!({"path":"A\\H"}));

        for (raw, expected) in [
            ("[1,2", json!([1, 2])),
            ("true", json!(true)),
            ("false", json!({})),
            ("12", json!(12)),
        ] {
            let mut builder =
                MessageBuilder::new(AssistantMessage::pending("api", "provider", "model", 1));
            builder
                .apply(&AssistantMessageEvent::ToolCallStart {
                    content_index: 0,
                    id: "call".into(),
                    tool_name: "run".into(),
                    namespace: None,
                })
                .unwrap();
            builder
                .apply(&AssistantMessageEvent::ToolCallDelta {
                    content_index: 0,
                    delta: raw.into(),
                })
                .unwrap();
            assert_eq!(arguments(&builder), &expected, "{raw}");
        }

        let mut builder =
            MessageBuilder::new(AssistantMessage::pending("api", "provider", "model", 1));
        builder
            .apply(&AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id: "call".into(),
                tool_name: "run".into(),
                namespace: None,
            })
            .unwrap();
        builder
            .apply(&AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: format!("{}0", "[".repeat(129)),
            })
            .unwrap();
        let mut nested = arguments(&builder);
        for _ in 0..129 {
            nested = &nested.as_array().expect("nested array")[0];
        }
        assert_eq!(nested, &json!(0));
    }

    /// Pins pi `src/api/anthropic-messages.ts:620-638,691-697` and
    /// `src/api/openai-responses-shared.ts:485-527` snapshot state.
    #[test]
    fn message_builder_carries_early_thinking_and_tool_namespace_state() {
        let mut builder =
            MessageBuilder::new(AssistantMessage::pending("api", "provider", "model", 1));
        builder
            .apply(&AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                thinking: Some("[Reasoning redacted]".into()),
                thinking_signature: Some("sig-".into()),
                redacted: Some(true),
            })
            .unwrap();
        builder
            .apply(&AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: String::new(),
                thinking_signature_delta: Some("tail".into()),
            })
            .unwrap();
        builder
            .apply(&AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "[Reasoning redacted]".into(),
                content_signature: None,
                redacted: None,
            })
            .unwrap();
        builder
            .apply(&AssistantMessageEvent::ToolCallStart {
                content_index: 1,
                id: "call|fc".into(),
                tool_name: "search".into(),
                namespace: Some("dynamic_tools".into()),
            })
            .unwrap();

        assert!(matches!(
            &builder.snapshot().content[0],
            AssistantContent::Thinking(block)
                if block.thinking == "[Reasoning redacted]"
                    && block.thinking_signature.as_deref() == Some("sig-tail")
                    && block.redacted == Some(true)
        ));
        assert!(matches!(
            &builder.snapshot().content[1],
            AssistantContent::ToolCall(call)
                if call.namespace.as_deref() == Some("dynamic_tools")
        ));
        assert_eq!(
            serde_json::to_value([
                AssistantMessageEvent::ThinkingStart {
                    content_index: 0,
                    thinking: Some("[Reasoning redacted]".into()),
                    thinking_signature: Some("sig".into()),
                    redacted: Some(true),
                },
                AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: String::new(),
                    thinking_signature_delta: Some("tail".into()),
                },
                AssistantMessageEvent::ToolCallStart {
                    content_index: 1,
                    id: "call".into(),
                    tool_name: "search".into(),
                    namespace: Some("dynamic_tools".into()),
                },
            ])
            .unwrap(),
            json!([
                {"type":"thinking_start","contentIndex":0,"thinking":"[Reasoning redacted]","thinkingSignature":"sig","redacted":true},
                {"type":"thinking_delta","contentIndex":0,"delta":"","thinkingSignatureDelta":"tail"},
                {"type":"toolcall_start","contentIndex":1,"id":"call","toolName":"search","namespace":"dynamic_tools"}
            ])
        );
    }

    /// Pins pi `types.ts:547-552` and `proxy.ts:353-362`: terminals replace the accumulated snapshot.
    #[test]
    fn message_builder_applies_done_and_error_terminals_authoritatively() {
        let base = AssistantMessage::pending("api", "provider", "model", 7);
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
            .apply(&AssistantMessageEvent::TextStart { content_index: 0 })
            .unwrap();
        error_builder
            .apply(&AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "partial".into(),
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
            reasoning_details: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Aborted,
            deferred: None,
            error_message: Some("Request aborted by user".into()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 7,
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

    /// Pins pi `types.ts:536-552` plus the partial-free fields in `proxy.ts:36-57`.
    #[test]
    fn event_wire_uses_pi_names_without_partial_snapshots() {
        let events = vec![
            AssistantMessageEvent::Start,
            AssistantMessageEvent::TextStart { content_index: 0 },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "a".into(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "a".into(),
                content_signature: Some("text-sig".into()),
            },
            AssistantMessageEvent::ThinkingStart {
                content_index: 1,
                thinking: None,
                thinking_signature: None,
                redacted: None,
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 1,
                delta: "b".into(),
                thinking_signature_delta: None,
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 1,
                content: "b".into(),
                content_signature: Some("thinking-sig".into()),
                redacted: Some(false),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 2,
                id: "call".into(),
                tool_name: "lookup".into(),
                namespace: None,
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2,
                delta: "{}".into(),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 2,
                tool_call: ToolCall::new("call", "lookup", json!({})),
            },
        ];
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            json!([
                {"type":"start"},
                {"type":"text_start","contentIndex":0},
                {"type":"text_delta","contentIndex":0,"delta":"a"},
                {"type":"text_end","contentIndex":0,"content":"a","contentSignature":"text-sig"},
                {"type":"thinking_start","contentIndex":1},
                {"type":"thinking_delta","contentIndex":1,"delta":"b"},
                {"type":"thinking_end","contentIndex":1,"content":"b","contentSignature":"thinking-sig","redacted":false},
                {"type":"toolcall_start","contentIndex":2,"id":"call","toolName":"lookup"},
                {"type":"toolcall_delta","contentIndex":2,"delta":"{}"},
                {"type":"toolcall_end","contentIndex":2,"toolCall":{"type":"toolCall","id":"call","name":"lookup","arguments":{}}}
            ])
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
