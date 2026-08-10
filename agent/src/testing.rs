//! Reusable, provider-free test support.
//!
//! Enable the `testing` crate feature to construct scripted assistant streams, record complete
//! provider requests and lifecycle events, and use deterministic fixture messages and tools without
//! contacting an external provider.

use crate::*;
use async_trait::async_trait;
use futures::Stream;
use genai::ModelSpec;
use genai::adapter::AdapterKind;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Channel- or iterator-backed assistant stream for parity and downstream tests.
///
/// It delegates terminal fusion and result handling to [`AssistantMessageEventStream`] while
/// providing constructors tailored to deterministic scripts.
pub struct ScriptedStream {
    inner: AssistantMessageEventStream,
}

/// A single queued provider response.
pub type Script = ScriptedStream;

impl std::fmt::Debug for ScriptedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedStream").finish_non_exhaustive()
    }
}

impl ScriptedStream {
    /// Expand a complete message into start, block-level, and terminal protocol events.
    pub fn from_message(message: AssistantMessage) -> Self {
        Self::from_events(events_for_message(message))
    }

    /// Yield an explicit event script in order.
    pub fn from_events(events: Vec<AssistantMessageEvent>) -> Self {
        Self {
            inner: AssistantMessageEventStream::from_events(events),
        }
    }

    /// Create an unbounded channel-backed script and its cloneable sender.
    pub fn channel() -> (AssistantStreamSender, Self) {
        let (sender, inner) = AssistantMessageEventStream::channel();
        (sender, Self { inner })
    }

    /// Spawn an asynchronous channel driver and return its stream immediately.
    ///
    /// This requires an active Tokio runtime. The driver owns the sender and may clone it for
    /// concurrent producers; dropping all senders without a terminal event produces a protocol
    /// error on the result handle once stream closure is observed.
    pub fn from_driver<F, Fut>(driver: F) -> Self
    where
        F: FnOnce(AssistantStreamSender) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (sender, stream) = Self::channel();
        tokio::spawn(async move { driver(sender).await });
        stream
    }

    /// Delay every protocol event. This is useful for deterministic abort/interleaving tests.
    pub fn with_delay(self, delay: Duration) -> Self {
        use futures::StreamExt;
        let delayed = self.inner.then(move |event| async move {
            tokio::time::sleep(delay).await;
            event
        });
        Self {
            inner: AssistantMessageEventStream::from_stream(delayed),
        }
    }

    /// Unwrap the underlying assistant event stream.
    pub fn into_inner(self) -> AssistantMessageEventStream {
        self.inner
    }

    /// Clone a handle for this script's first terminal message.
    pub fn result_handle(&self) -> AssistantMessageResult {
        self.inner.result_handle()
    }

    /// Await this script's first terminal message.
    ///
    /// Iterator-backed scripts still need to be consumed before or concurrently with this future.
    pub async fn result(&self) -> Result<AssistantMessage, StreamProtocolError> {
        self.inner.result().await
    }
}

impl From<AssistantMessage> for ScriptedStream {
    fn from(value: AssistantMessage) -> Self {
        Self::from_message(value)
    }
}

impl From<ScriptedStream> for AssistantMessageEventStream {
    fn from(value: ScriptedStream) -> Self {
        value.inner
    }
}

impl Stream for ScriptedStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Captured stream invocation without hiding any provider-boundary inputs.
pub type RecordedStreamCall = StreamRequest;

type StreamFactory = Arc<dyn Fn(StreamRequest) -> ScriptedStream + Send + Sync>;

/// Queue-backed or request-aware mock implementation of [`StreamFn`].
///
/// Every invocation is recorded before a response is selected. Queue mode consumes one script per
/// call in FIFO order; exhaustion returns an in-band error message. Factory mode invokes the same
/// synchronous request-aware factory for each call. Clones share both recordings and queued state.
#[derive(Clone)]
pub struct MockStreamFn {
    scripts: Option<Arc<Mutex<VecDeque<ScriptedStream>>>>,
    factory: Option<StreamFactory>,
    calls: Arc<Mutex<Vec<RecordedStreamCall>>>,
}

impl std::fmt::Debug for MockStreamFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockStreamFn")
            .field("queued_scripts", &self.remaining())
            .field("calls", &self.call_count())
            .finish_non_exhaustive()
    }
}

impl MockStreamFn {
    /// Construct a FIFO mock from scripted streams.
    pub fn from_streams(streams: Vec<ScriptedStream>) -> Self {
        Self {
            scripts: Some(Arc::new(Mutex::new(streams.into()))),
            factory: None,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Construct a FIFO mock by expanding each complete message into a script.
    pub fn from_messages(messages: Vec<AssistantMessage>) -> Self {
        Self::from_streams(
            messages
                .into_iter()
                .map(ScriptedStream::from_message)
                .collect(),
        )
    }

    /// Construct a request-aware mock that creates one script per invocation.
    pub fn from_fn(
        factory: impl Fn(StreamRequest) -> ScriptedStream + Send + Sync + 'static,
    ) -> Self {
        Self {
            scripts: None,
            factory: Some(Arc::new(factory)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Append a response in queue mode.
    ///
    /// Factory-backed mocks have no queue, so calling this method on one has no effect.
    pub fn push_stream(&self, stream: ScriptedStream) {
        if let Some(scripts) = &self.scripts {
            scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(stream);
        }
    }

    /// Return a cloned snapshot of all recorded requests in invocation order.
    pub fn calls(&self) -> Vec<RecordedStreamCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Return the number of recorded provider invocations.
    pub fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Return the number of queued responses, or zero in factory mode.
    pub fn remaining(&self) -> usize {
        self.scripts
            .as_ref()
            .map(|scripts| {
                scripts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
            })
            .unwrap_or(0)
    }
}

impl Default for MockStreamFn {
    fn default() -> Self {
        Self::from_streams(Vec::new())
    }
}

#[async_trait]
impl StreamFn for MockStreamFn {
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());

        if let Some(factory) = &self.factory {
            return factory(request).into_inner();
        }
        let script = self.scripts.as_ref().and_then(|scripts| {
            scripts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        });
        script.map(ScriptedStream::into_inner).unwrap_or_else(|| {
            AssistantMessageEventStream::from_error(AssistantMessage::error(
                crate::stream_fn::stream_error_model(&request.model),
                StopReason::Error,
                "MockStreamFn has no scripted response remaining",
            ))
        })
    }
}

/// Thread-safe ordered event collector and low-level event sink.
#[derive(Debug, Clone, Default)]
pub struct EventRecorder {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl EventRecorder {
    /// Create an empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an event to the shared ordered recording.
    pub fn record(&self, event: AgentEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    /// Return a cloned snapshot of all recorded events.
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Return the recorded event discriminants in order.
    pub fn kinds(&self) -> Vec<EventKind> {
        self.events().iter().map(AgentEvent::kind).collect()
    }

    /// Return a clone of the most recently recorded event.
    pub fn last(&self) -> Option<AgentEvent> {
        self.events().last().cloned()
    }

    /// Return clones of all events with the requested discriminant.
    pub fn matching(&self, kind: EventKind) -> Vec<AgentEvent> {
        self.events()
            .into_iter()
            .filter(|event| event.kind() == kind)
            .collect()
    }

    /// Remove all recorded events across every recorder clone.
    pub fn clear(&self) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Assert that the complete recorded kind sequence equals `expected`.
    ///
    /// # Panics
    ///
    /// Panics at the caller when the sequences differ.
    #[track_caller]
    pub fn assert_sequence(&self, expected: &[EventKind]) {
        assert_eq!(self.kinds(), expected);
    }

    /// Create an [`AgentListener`] that appends events to this recorder.
    pub fn listener(&self) -> AgentListener {
        let recorder = self.clone();
        Arc::new(move |event, _cancel| {
            let recorder = recorder.clone();
            Box::pin(async move { recorder.record(event) })
        })
    }
}

#[async_trait]
impl AgentEventSink for EventRecorder {
    async fn emit(&mut self, event: AgentEvent) {
        self.record(event);
    }
}

/// Standard-message converter for tests (custom messages are filtered out).
pub fn identity_convert_to_llm() -> ConvertToLlm {
    default_convert_to_llm()
}

/// Deterministic model, usage, and transcript fixtures.
pub mod fixtures {
    use super::*;

    /// Return the standard mock provider/model identity.
    pub fn model_iden() -> genai::ModelIden {
        genai::ModelIden::new(AdapterKind::OpenAIResp, "mock")
    }

    /// Return a model specification targeting [`model_iden`].
    pub fn model() -> ModelSpec {
        ModelSpec::from_iden(model_iden())
    }

    /// Return zeroed token usage.
    pub fn usage() -> AgentUsage {
        AgentUsage::default()
    }

    /// Return usage with the supplied input/output counts, no cache counts, and a derived total.
    pub fn usage_with(input_tokens: u64, output_tokens: u64) -> AgentUsage {
        AgentUsage::new(input_tokens, output_tokens)
    }

    /// Construct a transcript user message containing one text block.
    pub fn user_msg(text: impl Into<String>) -> AgentMessage {
        AgentMessage::User(UserMessage::text(text))
    }

    /// Construct an assistant message for the mock model with the supplied content and reason.
    pub fn assistant_msg(content: Vec<AssistantContent>, reason: StopReason) -> AssistantMessage {
        AssistantMessage::completed(model_iden(), content, reason)
    }

    /// Construct a normally stopped assistant message containing one text block.
    pub fn text_msg(text: impl Into<String>) -> AssistantMessage {
        assistant_msg(vec![AssistantContent::text(text)], StopReason::Stop)
    }

    /// Construct a tool-use assistant message containing the supplied calls.
    pub fn tool_use_msg(calls: Vec<AgentToolCall>) -> AssistantMessage {
        assistant_msg(
            calls.into_iter().map(AssistantContent::ToolCall).collect(),
            StopReason::ToolUse,
        )
    }

    /// Construct a successful transcript tool-result message containing one text block.
    pub fn tool_result_msg(
        id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage::text(id, name, text))
    }
}

/// Compact builders for scripted assistant content and responses.
pub mod script {
    use super::*;

    /// Construct an unsigned assistant text block.
    pub fn text(value: impl Into<String>) -> AssistantContent {
        AssistantContent::text(value)
    }

    /// Construct an unsigned assistant thinking block.
    pub fn thinking(value: impl Into<String>) -> AssistantContent {
        AssistantContent::thinking(value)
    }

    /// Construct an assistant tool-call block without thought signatures.
    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> AssistantContent {
        AssistantContent::ToolCall(AgentToolCall::new(id, name, arguments))
    }

    /// Expand supplied content and stop reason into a complete scripted stream.
    pub fn completed(content: Vec<AssistantContent>, reason: StopReason) -> ScriptedStream {
        ScriptedStream::from_message(fixtures::assistant_msg(content, reason))
    }

    /// Script a normally stopped response containing one text block.
    pub fn text_response(value: impl Into<String>) -> ScriptedStream {
        ScriptedStream::from_message(fixtures::text_msg(value))
    }

    /// Script a tool-use response containing the supplied calls.
    pub fn tool_call_turn(calls: Vec<AgentToolCall>) -> ScriptedStream {
        ScriptedStream::from_message(fixtures::tool_use_msg(calls))
    }

    /// Script an in-band error terminal message with no assistant content.
    pub fn in_band_error(message: impl Into<String>) -> ScriptedStream {
        ScriptedStream::from_message(AssistantMessage::error(
            fixtures::model_iden(),
            StopReason::Error,
            message,
        ))
    }

    /// Script an in-band aborted terminal message with no assistant content.
    pub fn aborted(message: impl Into<String>) -> ScriptedStream {
        ScriptedStream::from_message(AssistantMessage::error(
            fixtures::model_iden(),
            StopReason::Aborted,
            message,
        ))
    }
}

/// Provider-free example tools for end-to-end agent-loop tests.
pub mod tools {
    use super::*;

    /// Return a calculator tool for one whitespace-separated binary arithmetic operation.
    pub fn calculate_tool() -> Arc<dyn AgentTool> {
        let spec = ToolSpec::new(
            "calculate",
            "Evaluate a simple binary mathematical expression",
            json!({
                "type": "object",
                "properties": { "expression": { "type": "string" } },
                "required": ["expression"]
            }),
        )
        .with_label("Calculator");
        Arc::new(FnTool::from_value_fn(spec, |args| async move {
            let expression = args
                .get("expression")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing string `expression`".into()))?;
            let result = evaluate_binary_expression(expression)?;
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("{expression} = {result}"))],
                Value::Null,
            ))
        }))
    }

    /// Return a tool that reports the current Unix-epoch timestamp in milliseconds.
    pub fn current_time_tool() -> Arc<dyn AgentTool> {
        let spec = ToolSpec::new(
            "get_current_time",
            "Get the current UTC timestamp",
            json!({
                "type": "object",
                "properties": { "timezone": { "type": "string" } }
            }),
        )
        .with_label("Current Time");
        Arc::new(FnTool::from_value_fn(spec, |_args| async move {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            Ok(AgentToolResult::new(
                vec![ToolResultContent::text(format!("{millis}"))],
                json!({ "utcTimestamp": millis }),
            ))
        }))
    }

    fn evaluate_binary_expression(expression: &str) -> Result<f64, ToolError> {
        let mut parts = expression.split_whitespace();
        let lhs = parts
            .next()
            .ok_or_else(|| ToolError::InvalidArguments("empty expression".into()))?
            .parse::<f64>()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let operator = parts
            .next()
            .ok_or_else(|| ToolError::InvalidArguments("missing operator".into()))?;
        let rhs = parts
            .next()
            .ok_or_else(|| ToolError::InvalidArguments("missing right operand".into()))?
            .parse::<f64>()
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        if parts.next().is_some() {
            return Err(ToolError::InvalidArguments(
                "only a single binary operation is supported by the test tool".into(),
            ));
        }
        match operator {
            "+" => Ok(lhs + rhs),
            "-" => Ok(lhs - rhs),
            "*" => Ok(lhs * rhs),
            "/" => Ok(lhs / rhs),
            _ => Err(ToolError::InvalidArguments(format!(
                "unsupported operator `{operator}`"
            ))),
        }
    }
}

fn events_for_message(message: AssistantMessage) -> Vec<AssistantMessageEvent> {
    let mut partial = message.clone();
    partial.content.clear();
    partial.stop_reason = StopReason::Pending;
    partial.error_message = None;
    let mut events = vec![AssistantMessageEvent::Start {
        partial: partial.clone(),
    }];

    for (content_index, content) in message.content.iter().cloned().enumerate() {
        match content {
            AssistantContent::Text { text, signature } => {
                partial.content.push(AssistantContent::Text {
                    text: String::new(),
                    signature: signature.clone(),
                });
                events.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: partial.clone(),
                });
                if let Some(AssistantContent::Text { text: current, .. }) =
                    partial.content.get_mut(content_index)
                {
                    *current = text.clone();
                }
                events.push(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta: text.clone(),
                    partial: partial.clone(),
                });
                events.push(AssistantMessageEvent::TextEnd {
                    content_index,
                    content: text,
                    partial: partial.clone(),
                });
            }
            AssistantContent::Thinking {
                thinking,
                signature,
            } => {
                partial.content.push(AssistantContent::Thinking {
                    thinking: String::new(),
                    signature: signature.clone(),
                });
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: partial.clone(),
                });
                if let Some(AssistantContent::Thinking {
                    thinking: current, ..
                }) = partial.content.get_mut(content_index)
                {
                    *current = thinking.clone();
                }
                events.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: thinking.clone(),
                    partial: partial.clone(),
                });
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    thinking,
                    partial: partial.clone(),
                });
            }
            AssistantContent::ToolCall(call) => {
                events.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: partial.clone(),
                });
                events.push(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta: call.arguments.to_string(),
                    partial: partial.clone(),
                });
                partial
                    .content
                    .push(AssistantContent::ToolCall(call.clone()));
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call: call,
                    partial: partial.clone(),
                });
            }
        }
    }

    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        events.push(AssistantMessageEvent::Error {
            reason: message.stop_reason,
            error: message,
        });
    } else {
        events.push(AssistantMessageEvent::Done {
            reason: message.stop_reason,
            message,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn scripted_stream_exposes_events_and_terminal_result() {
        let expected = fixtures::text_msg("hello");
        let mut stream = ScriptedStream::from_message(expected.clone());
        let result = stream.result_handle();
        let events = stream.by_ref().collect::<Vec<_>>().await;

        assert_eq!(
            events.first().map(|event| event.partial().stop_reason),
            Some(StopReason::Pending)
        );
        assert_eq!(
            events
                .last()
                .and_then(AssistantMessageEvent::terminal_message),
            Some(&expected)
        );
        assert_eq!(result.get().await.unwrap(), expected);
    }

    #[tokio::test]
    async fn mock_stream_fn_records_requests_and_consumes_in_order() {
        let mock = MockStreamFn::from_messages(vec![fixtures::text_msg("one")]);
        let mut stream = mock
            .stream(StreamRequest::new(fixtures::model(), LlmContext::default()))
            .await;
        while stream.next().await.is_some() {}

        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.remaining(), 0);
    }

    #[test]
    fn event_recorder_asserts_order() {
        let recorder = EventRecorder::new();
        recorder.record(AgentEvent::AgentStart);
        recorder.record(AgentEvent::TurnStart);
        recorder.assert_sequence(&[EventKind::AgentStart, EventKind::TurnStart]);
    }

    #[test]
    fn update_sink_ignores_updates_after_close() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let captured = updates.clone();
        let sink = UpdateSink::new(move |update| captured.lock().unwrap().push(update));
        assert!(sink.emit(AgentToolResult::text("first")));
        sink.close();
        assert!(!sink.emit(AgentToolResult::text("late")));
        assert_eq!(updates.lock().unwrap().len(), 1);
    }
}
