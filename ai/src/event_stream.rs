//! Partial-free assistant events, terminal settlement, and snapshot reconstruction.

use crate::types::{
    AssistantContent, AssistantMessage, ErrorStopReason, SuccessfulStopReason, TextContent,
    ThinkingContent, ToolCall,
};
use futures::Stream;
use futures::stream::FusedStream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
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
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
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
            AssistantMessageEvent::ThinkingStart { content_index } => {
                self.set_content(
                    *content_index,
                    AssistantContent::Thinking(ThinkingContent::new("")),
                )?;
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
            } => match self.message.content.get_mut(*content_index) {
                Some(AssistantContent::Thinking(content)) => content.thinking.push_str(delta),
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
                    block.thinking_signature.clone_from(content_signature);
                    block.redacted = *redacted;
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
            } => {
                self.set_content(
                    *content_index,
                    AssistantContent::ToolCall(ToolCall::new(id, tool_name, Map::new())),
                )?;
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
                    call.arguments = parse_streaming_arguments(raw);
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

const MAX_STREAMING_JSON_DEPTH: usize = 128;

fn parse_streaming_arguments(raw: &str) -> Map<String, Value> {
    let raw = raw.trim();
    if raw.is_empty() || exceeds_streaming_json_depth(raw) {
        return Map::new();
    }
    if let Ok(Value::Object(value)) = serde_json::from_str(raw) {
        return value;
    }
    match PartialJsonParser::new(raw).parse_value() {
        Some(Value::Object(value)) => value,
        _ => Map::new(),
    }
}

fn exceeds_streaming_json_depth(input: &str) -> bool {
    let mut containers = [0_u8; MAX_STREAMING_JSON_DEPTH];
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in input.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else {
                match byte {
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                let Some(next_depth) = depth.checked_add(1) else {
                    return true;
                };
                if next_depth > MAX_STREAMING_JSON_DEPTH {
                    return true;
                }
                containers[depth] = byte;
                depth = next_depth;
            }
            b'}' if depth > 0 && containers[depth - 1] == b'{' => depth -= 1,
            b']' if depth > 0 && containers[depth - 1] == b'[' => depth -= 1,
            _ => {}
        }
    }

    false
}

struct PartialJsonParser {
    chars: Vec<char>,
    cursor: usize,
}

impl PartialJsonParser {
    fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            cursor: 0,
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        match self.peek()? {
            '{' => Some(Value::Object(self.parse_object())),
            '[' => Some(Value::Array(self.parse_array())),
            '"' => Some(Value::String(self.parse_string().0)),
            't' => self.parse_literal("true", Value::Bool(true)),
            'f' => self.parse_literal("false", Value::Bool(false)),
            'n' => self.parse_literal("null", Value::Null),
            '-' | '0'..='9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Map<String, Value> {
        self.bump();
        let mut object = Map::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some('}') => {
                    self.bump_if('}');
                    break;
                }
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some('"') => {}
                Some(_) => {
                    self.skip_to_member_boundary();
                    continue;
                }
            }

            let (key, key_closed) = self.parse_string();
            self.skip_whitespace();
            if !key_closed || !self.bump_if(':') {
                break;
            }
            self.skip_whitespace();
            let Some(value) = self.parse_value() else {
                break;
            };
            object.insert(key, value);

            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                None => break,
                Some(_) => self.skip_to_member_boundary(),
            }
        }
        object
    }

    fn parse_array(&mut self) -> Vec<Value> {
        self.bump();
        let mut array = Vec::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some(']') => {
                    self.bump_if(']');
                    break;
                }
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some(_) => {}
            }
            let Some(value) = self.parse_value() else {
                break;
            };
            array.push(value);
            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                None => break,
                Some(_) => self.skip_to_member_boundary(),
            }
        }
        array
    }

    fn parse_string(&mut self) -> (String, bool) {
        if !self.bump_if('"') {
            return (String::new(), false);
        }
        let mut output = String::new();
        while let Some(character) = self.bump() {
            match character {
                '"' => return (output, true),
                '\\' => match self.bump() {
                    Some('"') => output.push('"'),
                    Some('\\') => output.push('\\'),
                    Some('/') => output.push('/'),
                    Some('b') => output.push('\u{0008}'),
                    Some('f') => output.push('\u{000c}'),
                    Some('n') => output.push('\n'),
                    Some('r') => output.push('\r'),
                    Some('t') => output.push('\t'),
                    Some('u') => self.parse_unicode_escape(&mut output),
                    Some(other) => {
                        output.push('\\');
                        output.push(other);
                    }
                    None => output.push('\\'),
                },
                other => output.push(other),
            }
        }
        (output, false)
    }

    fn parse_unicode_escape(&mut self, output: &mut String) {
        let start = self.cursor;
        let mut digits = String::new();
        for _ in 0..4 {
            match self.peek() {
                Some(character) if character.is_ascii_hexdigit() => {
                    digits.push(character);
                    self.bump();
                }
                _ => break,
            }
        }
        if digits.len() == 4
            && let Ok(codepoint) = u32::from_str_radix(&digits, 16)
            && let Some(character) = char::from_u32(codepoint)
        {
            output.push(character);
        } else {
            self.cursor = start;
            output.push_str("\\u");
        }
    }

    fn parse_literal(&mut self, expected: &str, value: Value) -> Option<Value> {
        let remaining = self.chars[self.cursor..].iter().collect::<String>();
        let token = remaining
            .chars()
            .take_while(|character| character.is_ascii_alphabetic())
            .collect::<String>();
        if expected.starts_with(&token) && !token.is_empty() {
            self.cursor += token.chars().count();
            Some(value)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Value> {
        let start = self.cursor;
        while matches!(self.peek(), Some('-' | '+' | '.' | 'e' | 'E' | '0'..='9')) {
            self.bump();
        }
        let mut token = self.chars[start..self.cursor].iter().collect::<String>();
        while !token.is_empty() {
            if let Ok(number) = token.parse::<Number>() {
                return Some(Value::Number(number));
            }
            token.pop();
        }
        None
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn skip_to_member_boundary(&mut self) {
        while let Some(character) = self.peek() {
            if character == ',' {
                break;
            }
            if matches!(character, '}' | ']') {
                self.bump();
                break;
            }
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.cursor).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.cursor += 1;
        Some(value)
    }

    fn bump_if(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
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
        let mut first_arguments = Map::new();
        first_arguments.insert("city".into(), json!("Paris"));
        let mut second_arguments = Map::new();
        second_arguments.insert("days".into(), json!(2));
        let events = vec![
            AssistantMessageEvent::Start,
            AssistantMessageEvent::ThinkingStart { content_index: 0 },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "plan".into(),
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
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 3,
                id: "b".into(),
                tool_name: "calendar".into(),
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
            AssistantMessageEvent::ThinkingStart { content_index: 1 },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 1,
                delta: "b".into(),
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
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2,
                delta: "{}".into(),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 2,
                tool_call: ToolCall::new("call", "lookup", Map::new()),
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
