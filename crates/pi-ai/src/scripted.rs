//! Hermetic fake model runtime from Architecture v2 part 1 §8 and §10 M1.

use crate::{
    ApiId, AssemblyError, AssistantAssembler, AssistantEvent, AssistantFinish,
    AssistantFinishReason, AssistantStream, CancellationReason, CancellationToken, ContentBlockId,
    ContentBlockKind, LocalAssistantStream, LocalBoxFuture, LocalModelRuntime, MessageId,
    ModelRequest, ModelRuntime, OpaquePayload, PublicError, ReplayApplicability,
    ReplayDataOperation, ReplayItemId, ReplayKind, ReplayTarget, RequestStartError,
    RequestStartErrorKind, SendBoxFuture, Timestamp, ToolCallId, Usage,
};
use futures_core::Stream;
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

/// Replay target expressed in stable generated-response indexes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedReplayTarget {
    /// The assistant message as a whole.
    Message,
    /// A generated canonical block by zero-based content index.
    ContentBlock(u32),
    /// A generated tool call by zero-based tool-call index.
    ToolCall(u32),
    /// An API-family output item with its original output index.
    ProviderOutputItem {
        /// Provider output index.
        output_index: u32,
    },
}

/// One replay item emitted by a generated scripted response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptedReplayItem {
    /// Stable replay-item identifier.
    pub id: ReplayItemId,
    /// Original provider output ordinal.
    pub ordinal: u32,
    /// Generated response target.
    pub target: ScriptedReplayTarget,
    /// Open API-family replay kind.
    pub kind: ReplayKind,
    /// Scope in which an encoder may reuse the value.
    pub applicability: ReplayApplicability,
    /// Exact opaque payload.
    pub payload: OpaquePayload,
}

/// One response consumed by [`ScriptedRuntime`].
#[derive(Clone, Debug)]
pub struct ScriptedResponse {
    kind: ScriptedResponseKind,
    api: ApiId,
    response_id: Option<String>,
    response_model: Option<crate::ModelId>,
    usage_updates: Vec<Usage>,
    replay_items: Vec<ScriptedReplayItem>,
    timestamp: Timestamp,
}

#[derive(Clone, Debug)]
enum ScriptedResponseKind {
    Generated {
        content: GeneratedContent,
        terminal: ScriptedTerminal,
    },
    Events(Vec<AssistantEvent>),
}

#[derive(Clone, Debug)]
enum GeneratedContent {
    Empty,
    Text(String),
    ToolCall { name: String, arguments: Value },
}

#[derive(Clone, Debug)]
enum ScriptedTerminal {
    Completed(AssistantFinishReason),
    Failed(PublicError),
    Cancelled(CancellationReason),
}

impl ScriptedResponse {
    /// Uses an exact normalized event sequence.
    ///
    /// The first event must be `MessageStarted`. A missing provider terminal is
    /// converted to a failed terminal message, matching part 2 §10.1.
    pub fn events(events: impl IntoIterator<Item = AssistantEvent>) -> Self {
        Self {
            kind: ScriptedResponseKind::Events(events.into_iter().collect()),
            ..Self::empty_completed()
        }
    }

    /// Builds and validates a successful terminal around exact nonterminal
    /// events. This is convenient for replay sequences in part 2 §1.4–§1.8.
    pub fn completed_events(
        events: impl IntoIterator<Item = AssistantEvent>,
        finish: AssistantFinish,
    ) -> Result<Self, AssemblyError> {
        let mut events = events.into_iter().collect::<Vec<_>>();
        let mut assembler = AssistantAssembler::new();
        for event in &events {
            assembler.apply(event)?;
        }
        let message = assembler.finish_completed(finish)?;
        events.push(AssistantEvent::Finished { message });
        Ok(Self::events(events))
    }

    /// Creates an empty failed response.
    pub fn failure(error: PublicError) -> Self {
        Self {
            kind: ScriptedResponseKind::Generated {
                content: GeneratedContent::Empty,
                terminal: ScriptedTerminal::Failed(error),
            },
            ..Self::empty_completed()
        }
    }

    /// Creates an empty cancelled response.
    pub fn cancellation(reason: CancellationReason) -> Self {
        Self {
            kind: ScriptedResponseKind::Generated {
                content: GeneratedContent::Empty,
                terminal: ScriptedTerminal::Cancelled(reason),
            },
            ..Self::empty_completed()
        }
    }

    /// Overrides the API-family identity emitted by a generated response.
    pub fn with_api(mut self, api: impl Into<ApiId>) -> Self {
        self.api = api.into();
        self
    }

    /// Adds response ID and concrete model metadata.
    pub fn with_response_metadata(
        mut self,
        response_id: Option<String>,
        response_model: Option<crate::ModelId>,
    ) -> Self {
        self.response_id = response_id;
        self.response_model = response_model;
        self
    }

    /// Adds a cumulative usage update to a generated response.
    pub fn with_usage(mut self, cumulative: Usage) -> Self {
        self.usage_updates.push(cumulative);
        self
    }

    /// Adds a replay item to a generated response.
    pub fn with_replay_item(mut self, item: ScriptedReplayItem) -> Self {
        self.replay_items.push(item);
        self
    }

    /// Makes a generated response terminate with a structured failure after
    /// emitting its configured partial content.
    pub fn failing(mut self, error: PublicError) -> Self {
        if let ScriptedResponseKind::Generated { terminal, .. } = &mut self.kind {
            *terminal = ScriptedTerminal::Failed(error);
        }
        self
    }

    /// Makes a generated response terminate with cancellation after emitting
    /// its configured partial content.
    pub fn cancelling(mut self, reason: CancellationReason) -> Self {
        if let ScriptedResponseKind::Generated { terminal, .. } = &mut self.kind {
            *terminal = ScriptedTerminal::Cancelled(reason);
        }
        self
    }

    /// Sets the deterministic message timestamp for a generated response.
    pub fn with_timestamp(mut self, timestamp: Timestamp) -> Self {
        self.timestamp = timestamp;
        self
    }

    fn empty_completed() -> Self {
        Self {
            kind: ScriptedResponseKind::Generated {
                content: GeneratedContent::Empty,
                terminal: ScriptedTerminal::Completed(AssistantFinishReason::Stop),
            },
            api: ApiId::new("scripted"),
            response_id: None,
            response_model: None,
            usage_updates: Vec::new(),
            replay_items: Vec::new(),
            timestamp: Timestamp::default(),
        }
    }
}

/// Creates a successful single-text-block scripted response.
pub fn text_response(text: impl Into<String>) -> ScriptedResponse {
    ScriptedResponse {
        kind: ScriptedResponseKind::Generated {
            content: GeneratedContent::Text(text.into()),
            terminal: ScriptedTerminal::Completed(AssistantFinishReason::Stop),
        },
        ..ScriptedResponse::empty_completed()
    }
}

/// Creates a successful one-tool-call scripted response.
pub fn tool_call_response(name: impl Into<String>, arguments: Value) -> ScriptedResponse {
    ScriptedResponse {
        kind: ScriptedResponseKind::Generated {
            content: GeneratedContent::ToolCall {
                name: name.into(),
                arguments,
            },
            terminal: ScriptedTerminal::Completed(AssistantFinishReason::ToolUse),
        },
        ..ScriptedResponse::empty_completed()
    }
}

/// Builder for a queue-backed [`ScriptedRuntime`].
#[derive(Clone, Debug, Default)]
pub struct ScriptedRuntimeBuilder {
    responses: Vec<ScriptedResponse>,
}

impl ScriptedRuntimeBuilder {
    /// Appends one scripted response.
    pub fn response(mut self, response: ScriptedResponse) -> Self {
        self.responses.push(response);
        self
    }

    /// Appends one empty structured failure.
    pub fn failure(self, error: PublicError) -> Self {
        self.response(ScriptedResponse::failure(error))
    }

    /// Appends one empty structured cancellation.
    pub fn cancellation(self, reason: CancellationReason) -> Self {
        self.response(ScriptedResponse::cancellation(reason))
    }

    /// Appends an exact replay-aware event sequence.
    pub fn scripted_events(self, events: impl IntoIterator<Item = AssistantEvent>) -> Self {
        self.response(ScriptedResponse::events(events))
    }

    /// Appends an empty successful response with one cumulative usage update.
    pub fn usage(self, cumulative: Usage) -> Self {
        self.response(ScriptedResponse::empty_completed().with_usage(cumulative))
    }

    /// Builds the runtime with responses consumed in insertion order.
    pub fn build(self) -> ScriptedRuntime {
        ScriptedRuntime::new(self.responses)
    }
}

/// Thread-safe deterministic fake implementing both runtime families.
#[derive(Clone, Debug)]
pub struct ScriptedRuntime {
    inner: Arc<Mutex<ScriptedRuntimeState>>,
}

#[derive(Debug)]
struct ScriptedRuntimeState {
    responses: VecDeque<ScriptedResponse>,
    next_message_sequence: u64,
}

impl ScriptedRuntime {
    /// Creates a runtime from responses consumed in iteration order.
    pub fn new(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ScriptedRuntimeState {
                responses: responses.into_iter().collect(),
                next_message_sequence: 1,
            })),
        }
    }

    /// Starts an empty runtime builder.
    pub fn builder() -> ScriptedRuntimeBuilder {
        ScriptedRuntimeBuilder::default()
    }

    /// Returns the number of unconsumed scripted responses.
    pub fn remaining(&self) -> usize {
        lock_unpoisoned(&self.inner).responses.len()
    }

    fn prepare(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ScriptedEventStream, RequestStartError> {
        let (response, sequence) = {
            let mut state = lock_unpoisoned(&self.inner);
            let response = state.responses.pop_front().ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    "scripted runtime has no remaining response",
                )
                .with_model(request.model.clone())
            })?;
            let sequence = state.next_message_sequence;
            state.next_message_sequence = state.next_message_sequence.saturating_add(1);
            (response, sequence)
        };

        let events = materialize(response, &request, sequence)?;
        Ok(ScriptedEventStream::new(events, cancellation))
    }
}

impl ModelRuntime for ScriptedRuntime {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>> {
        Box::pin(async move {
            self.prepare(request, cancellation)
                .map(AssistantStream::new)
        })
    }
}

impl LocalModelRuntime for ScriptedRuntime {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async move {
            self.prepare(request, cancellation)
                .map(LocalAssistantStream::new)
        })
    }
}

fn materialize(
    response: ScriptedResponse,
    request: &ModelRequest,
    sequence: u64,
) -> Result<Vec<AssistantEvent>, RequestStartError> {
    if let ScriptedResponseKind::Events(events) = response.kind {
        return validate_or_close_premature(events, request);
    }

    let ScriptedResponseKind::Generated { content, terminal } = response.kind else {
        unreachable!("event responses returned above")
    };
    let mut assembler = AssistantAssembler::with_timestamp(response.timestamp);
    let mut events = Vec::new();
    apply_and_push(
        &mut assembler,
        &mut events,
        AssistantEvent::MessageStarted {
            message_id: MessageId::new(format!("scripted-message-{sequence}")),
            provider: request.model.provider.clone(),
            api: response.api,
            model: request.model.model.clone(),
        },
    )?;
    if response.response_id.is_some() || response.response_model.is_some() {
        apply_and_push(
            &mut assembler,
            &mut events,
            AssistantEvent::ResponseMetadata {
                response_id: response.response_id,
                response_model: response.response_model,
                end_turn: None,
            },
        )?;
    }

    let content_is_empty = matches!(content, GeneratedContent::Empty);
    match content {
        GeneratedContent::Empty => {}
        GeneratedContent::Text(text) => {
            let block_id = generated_block_id(0);
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index: 0,
                    kind: ContentBlockKind::Text,
                },
            )?;
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::TextDelta {
                    block_id: block_id.clone(),
                    delta: text,
                },
            )?;
            emit_replay_items(&mut assembler, &mut events, &response.replay_items)?;
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ContentBlockFinished { block_id },
            )?;
        }
        GeneratedContent::ToolCall { name, arguments } => {
            let block_id = generated_block_id(0);
            let call_id = generated_tool_call_id(0);
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index: 0,
                    kind: ContentBlockKind::ToolCall,
                },
            )?;
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ToolCallMetadata {
                    block_id: block_id.clone(),
                    call_id,
                    name: Some(name),
                },
            )?;
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ToolArgumentsDelta {
                    block_id: block_id.clone(),
                    delta: serde_json::to_string(&arguments).map_err(|error| {
                        invalid_script(request, format!("could not encode tool arguments: {error}"))
                    })?,
                },
            )?;
            emit_replay_items(&mut assembler, &mut events, &response.replay_items)?;
            apply_and_push(
                &mut assembler,
                &mut events,
                AssistantEvent::ContentBlockFinished { block_id },
            )?;
        }
    }

    if content_is_empty {
        emit_replay_items(&mut assembler, &mut events, &response.replay_items)?;
    }
    for cumulative in response.usage_updates {
        apply_and_push(
            &mut assembler,
            &mut events,
            AssistantEvent::UsageUpdated { cumulative },
        )?;
    }

    let terminal_event = match terminal {
        ScriptedTerminal::Completed(reason) => {
            let message = assembler
                .clone()
                .finish_completed(AssistantFinish {
                    reason,
                    raw_provider_reason: None,
                    error: None,
                })
                .map_err(|error| invalid_script(request, error.to_string()))?;
            AssistantEvent::Finished { message }
        }
        ScriptedTerminal::Failed(error) => AssistantEvent::Failed {
            message: assembler
                .clone()
                .finish_failed(error.sanitized(&request_secret_values(request)), None),
        },
        ScriptedTerminal::Cancelled(reason) => AssistantEvent::Cancelled {
            message: assembler.clone().finish_cancelled(reason),
        },
    };
    apply_and_push(&mut assembler, &mut events, terminal_event)?;
    Ok(events)
}

fn emit_replay_items(
    assembler: &mut AssistantAssembler,
    events: &mut Vec<AssistantEvent>,
    items: &[ScriptedReplayItem],
) -> Result<(), RequestStartError> {
    for item in items {
        let target = match item.target {
            ScriptedReplayTarget::Message => ReplayTarget::Message,
            ScriptedReplayTarget::ContentBlock(index) => {
                ReplayTarget::ContentBlock(generated_block_id(index))
            }
            ScriptedReplayTarget::ToolCall(index) => {
                ReplayTarget::ToolCall(generated_tool_call_id(index))
            }
            ScriptedReplayTarget::ProviderOutputItem { output_index } => {
                ReplayTarget::ProviderOutputItem { output_index }
            }
        };
        apply_and_push(
            assembler,
            events,
            AssistantEvent::ReplayItemStarted {
                item_id: item.id.clone(),
                ordinal: item.ordinal,
                target,
                kind: item.kind.clone(),
                applicability: item.applicability,
            },
        )?;
        let operation = match &item.payload {
            OpaquePayload::Utf8(value) => ReplayDataOperation::ReplaceUtf8(value.clone()),
            OpaquePayload::Bytes(value) => ReplayDataOperation::ReplaceBytes(value.clone()),
            OpaquePayload::JsonBytes(value) => ReplayDataOperation::ReplaceJsonBytes(value.clone()),
        };
        apply_and_push(
            assembler,
            events,
            AssistantEvent::ReplayData {
                item_id: item.id.clone(),
                operation,
            },
        )?;
        apply_and_push(
            assembler,
            events,
            AssistantEvent::ReplayItemFinished {
                item_id: item.id.clone(),
            },
        )?;
    }
    Ok(())
}

fn validate_or_close_premature(
    mut events: Vec<AssistantEvent>,
    request: &ModelRequest,
) -> Result<Vec<AssistantEvent>, RequestStartError> {
    if !matches!(events.first(), Some(AssistantEvent::MessageStarted { .. })) {
        return Err(invalid_script(
            request,
            "exact scripted event sequence must begin with MessageStarted",
        ));
    }
    let mut assembler = AssistantAssembler::new();
    let mut terminal = false;
    for event in &events {
        assembler
            .apply(event)
            .map_err(|error| invalid_script(request, error.to_string()))?;
        terminal |= event.is_terminal();
    }
    if !terminal {
        let message = assembler.finish_failed(
            PublicError {
                code: "missing_provider_terminal".into(),
                message: "scripted provider stream ended without a terminal event".into(),
                retryable: false,
                provider_code: None,
                status: None,
                request_id: None,
            },
            None,
        );
        events.push(AssistantEvent::Failed { message });
    }
    Ok(events)
}

fn apply_and_push(
    assembler: &mut AssistantAssembler,
    events: &mut Vec<AssistantEvent>,
    event: AssistantEvent,
) -> Result<(), RequestStartError> {
    assembler.apply(&event).map_err(|error| {
        RequestStartError::new(RequestStartErrorKind::Internal, error.to_string())
    })?;
    events.push(event);
    Ok(())
}

fn invalid_script(request: &ModelRequest, message: impl Into<String>) -> RequestStartError {
    RequestStartError::new(RequestStartErrorKind::InvalidRequest, message)
        .with_model(request.model.clone())
}

fn generated_block_id(index: u32) -> ContentBlockId {
    ContentBlockId::new(format!("scripted-block-{index}"))
}

fn generated_tool_call_id(index: u32) -> ToolCallId {
    ToolCallId::new(format!("scripted-call-{index}"))
}

fn request_secret_values(request: &ModelRequest) -> Vec<&str> {
    request
        .options
        .headers
        .iter()
        .filter(|(name, _)| is_sensitive_header(name))
        .filter_map(|(_, value)| value.as_deref())
        .collect()
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
            | "cookie"
            | "set-cookie"
    )
}

struct ScriptedEventStream {
    events: VecDeque<AssistantEvent>,
    cancellation: CancellationToken,
    assembler: AssistantAssembler,
    started: bool,
    done: bool,
}

impl ScriptedEventStream {
    fn new(events: Vec<AssistantEvent>, cancellation: CancellationToken) -> Self {
        Self {
            events: events.into(),
            cancellation,
            assembler: AssistantAssembler::new(),
            started: false,
            done: false,
        }
    }
}

impl Stream for ScriptedEventStream {
    type Item = AssistantEvent;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        if self.started && self.cancellation.is_cancelled() {
            let event = AssistantEvent::Cancelled {
                message: self
                    .assembler
                    .clone()
                    .finish_cancelled(CancellationReason::new("Request was aborted")),
            };
            if self.assembler.apply(&event).is_err() {
                self.done = true;
                return Poll::Ready(None);
            }
            self.done = true;
            return Poll::Ready(Some(event));
        }

        let Some(event) = self.events.pop_front() else {
            self.done = true;
            return Poll::Ready(None);
        };
        if self.assembler.apply(&event).is_err() {
            self.done = true;
            return Poll::Ready(None);
        }
        self.started |= matches!(event, AssistantEvent::MessageStarted { .. });
        self.done = event.is_terminal();
        Poll::Ready(Some(event))
    }
}

impl fmt::Debug for ScriptedEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptedEventStream")
            .field("remaining", &self.events.len())
            .field("started", &self.started)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
