//! Google GenerateContent SSE decoding into replay-aware assistant events.

use pi_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    CacheWriteRetention, CancellationReason, ContentBlockId, ContentBlockKind, Currency,
    GOOGLE_THOUGHT_SIGNATURE_KIND, MessageId, ModelId, ModelPricing, ProviderId, PublicError,
    ReplayApplicability, ReplayDataOperation, ReplayItemId, ReplayKind, ReplayTarget, Timestamp,
    ToolCallId, Usage, UsageSource,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// Pinned Pi keeps one module-scoped fallback tool-call counter in each Google
// API-family module. Atomics preserve that family-local process lifetime while
// preventing separate concurrent decoders from reusing the same sequence.
static GOOGLE_GENERATIVE_AI_TOOL_CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static GOOGLE_VERTEX_TOOL_CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Stable identity inputs for a Google decoder instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDecodeContext {
    /// Stable canonical message identifier.
    pub message_id: MessageId,
    /// Provider serving the request.
    pub provider: ProviderId,
    /// Google API family serving the request.
    pub api: ApiId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Requested-model pricing used for Pi-equivalent terminal cost.
    pub pricing: ModelPricing,
    /// Timestamp retained by the assembled message.
    pub timestamp: Timestamp,
}

/// A malformed or inconsistent Google stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDecodeError {
    message: String,
}

impl GoogleDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GoogleDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GoogleDecodeError {}

/// Decodes a complete Google SSE transcript and always emits one terminal
/// assistant event.
pub fn decode_google_sse(body: &[u8], context: GoogleDecodeContext) -> Vec<AssistantEvent> {
    let mut decoder = GoogleSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.push(body));
    events.extend(decoder.finish());
    events
}

/// Incremental decoder shared by Gemini Developer API and Vertex.
pub struct GoogleSseDecoder {
    state: DecodeState,
    buffer: Vec<u8>,
    record: Vec<u8>,
    initial_bom_pending: bool,
    terminated: bool,
}

impl GoogleSseDecoder {
    /// Creates a decoder with its stable `MessageStarted` event queued.
    pub fn new(context: GoogleDecodeContext) -> Self {
        Self {
            state: DecodeState::new(context),
            buffer: Vec::new(),
            record: Vec::new(),
            initial_bom_pending: true,
            terminated: false,
        }
    }

    /// Drains events emitted since the preceding call.
    pub fn take_events(&mut self) -> Vec<AssistantEvent> {
        std::mem::take(&mut self.state.events)
    }

    /// Decodes every complete SSE record in a body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        if self.initial_bom_pending {
            const BOM: &[u8] = b"\xef\xbb\xbf";
            if self.buffer.len() < BOM.len() && BOM.starts_with(&self.buffer) {
                return self.take_events();
            }
            if self.buffer.starts_with(BOM) {
                self.buffer.drain(..BOM.len());
            }
            self.initial_bom_pending = false;
        }
        while let Some((end, separator)) = find_line_boundary(&self.buffer) {
            let line = self.buffer[..end].to_vec();
            self.buffer.drain(..end + separator);
            if let Err(error) = self.consume_line(&line) {
                self.state.fail(error.to_string());
                self.terminated = true;
                break;
            }
        }
        self.take_events()
    }

    /// Completes body decoding and validates a provider finish reason.
    pub fn finish(&mut self) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Err(error) = self.consume_line(&line) {
                self.state.fail(error.to_string());
                self.terminated = true;
                return self.take_events();
            }
        }
        if let Err(error) = self.flush_record() {
            self.state.fail(error.to_string());
            self.terminated = true;
            return self.take_events();
        }
        self.state.finish();
        self.terminated = true;
        self.take_events()
    }

    /// Converts a post-establishment transport failure into a committed
    /// failed assistant message.
    pub fn fail_transport(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.state.fail_public(PublicError {
            code: code.into(),
            message: message.into(),
            retryable: false,
            provider_code: None,
            status: None,
            request_id: self.state.response_id.clone(),
        });
        self.terminated = true;
        self.take_events()
    }

    /// Converts cancellation into a committed partial assistant message.
    pub fn cancel(&mut self, message: impl Into<String>) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.state.cancel(CancellationReason {
            message: message.into(),
            request_id: self.state.response_id.clone(),
        });
        self.terminated = true;
        self.take_events()
    }

    /// Whether the decoder already emitted a terminal event.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    fn consume_line(&mut self, line: &[u8]) -> Result<(), GoogleDecodeError> {
        if line.is_empty() {
            return self.flush_record();
        }
        if !self.record.is_empty() {
            self.record.push(b'\n');
        }
        self.record.extend_from_slice(line);
        Ok(())
    }

    fn flush_record(&mut self) -> Result<(), GoogleDecodeError> {
        if self.record.is_empty() {
            return Ok(());
        }
        let record = std::mem::take(&mut self.record);
        self.state.decode_sse_event(&record)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActiveKind {
    Text,
    Thinking,
}

#[derive(Clone)]
struct ActiveBlock {
    kind: ActiveKind,
    block_id: ContentBlockId,
    replay_item: Option<ReplayItemId>,
}

struct DecodeState {
    context: GoogleDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    active: Option<ActiveBlock>,
    next_content_index: u32,
    next_provider_ordinal: u32,
    used_tool_ids: BTreeSet<ToolCallId>,
    response_id: Option<String>,
    raw_finish_reason: Option<String>,
    saw_tool_call: bool,
}

impl DecodeState {
    fn new(context: GoogleDecodeContext) -> Self {
        let mut state = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            events: Vec::new(),
            active: None,
            next_content_index: 0,
            next_provider_ordinal: 0,
            used_tool_ids: BTreeSet::new(),
            response_id: None,
            raw_finish_reason: None,
            saw_tool_call: false,
            context,
        };
        state
            .emit(AssistantEvent::MessageStarted {
                message_id: state.context.message_id.clone(),
                provider: state.context.provider.clone(),
                api: state.context.api.clone(),
                model: state.context.requested_model.clone(),
            })
            .expect("MessageStarted is valid on a fresh assembler");
        state
    }

    fn decode_sse_event(&mut self, record: &[u8]) -> Result<(), GoogleDecodeError> {
        let source = std::str::from_utf8(record)
            .map_err(|error| GoogleDecodeError::new(format!("SSE body is not UTF-8: {error}")))?;
        let data = sse_data(source);
        let Some(data) = data else {
            return Ok(());
        };
        let chunk: Value = serde_json::from_str(&data)
            .map_err(|error| GoogleDecodeError::new(format!("invalid Google SSE JSON: {error}")))?;
        let chunk = chunk
            .as_object()
            .ok_or_else(|| GoogleDecodeError::new("Google SSE data is not an object"))?;
        if let Some(response_id) =
            optional_string(chunk, "responseId")?.filter(|value| !value.is_empty())
            && self.response_id.is_none()
        {
            self.response_id = Some(response_id.to_owned());
            self.emit(AssistantEvent::ResponseMetadata {
                response_id: Some(response_id.to_owned()),
                response_model: None,
                end_turn: None,
            })?;
        }
        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(Value::as_object);
        if let Some(parts) = candidate
            .and_then(|candidate| candidate.get("content"))
            .and_then(Value::as_object)
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                let part = part.as_object().ok_or_else(|| {
                    GoogleDecodeError::new("Google content part is not an object")
                })?;
                // R7 ordinals describe provider-part order, not canonical
                // block order. Consecutive Google text parts may merge into
                // one canonical block, but every unsigned part must still
                // advance the ordinal seen by a later signed part.
                let provider_ordinal = self.take_provider_ordinal();
                if let Some(text) = optional_string(part, "text")? {
                    let kind = if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        ActiveKind::Thinking
                    } else {
                        ActiveKind::Text
                    };
                    self.append_text_part(
                        kind,
                        text,
                        optional_string(part, "thoughtSignature")?,
                        provider_ordinal,
                    )?;
                }
                if let Some(function) = part.get("functionCall") {
                    self.close_active()?;
                    self.function_call(
                        function.as_object().ok_or_else(|| {
                            GoogleDecodeError::new("Google functionCall is not an object")
                        })?,
                        optional_string(part, "thoughtSignature")?,
                        provider_ordinal,
                    )?;
                }
            }
        }
        if let Some(reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            self.raw_finish_reason = Some(reason.to_owned());
        }
        if let Some(usage) = chunk.get("usageMetadata") {
            self.update_usage(
                usage
                    .as_object()
                    .ok_or_else(|| GoogleDecodeError::new("usageMetadata is not an object"))?,
            )?;
        }
        Ok(())
    }

    fn append_text_part(
        &mut self,
        kind: ActiveKind,
        text: &str,
        signature: Option<&str>,
        provider_ordinal: u32,
    ) -> Result<(), GoogleDecodeError> {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.kind != kind)
        {
            self.close_active()?;
            let block_id = ContentBlockId::new(format!(
                "google-block-{}-{}",
                self.context.message_id, self.next_content_index
            ));
            self.emit(AssistantEvent::ContentBlockStarted {
                block_id: block_id.clone(),
                content_index: self.next_content_index,
                kind: match kind {
                    ActiveKind::Text => ContentBlockKind::Text,
                    ActiveKind::Thinking => ContentBlockKind::Thinking,
                },
            })?;
            self.next_content_index = self.next_content_index.saturating_add(1);
            self.active = Some(ActiveBlock {
                kind,
                block_id,
                replay_item: None,
            });
        }
        let block_id = self
            .active
            .as_ref()
            .expect("created above")
            .block_id
            .clone();
        self.emit(match kind {
            ActiveKind::Text => AssistantEvent::TextDelta {
                block_id,
                delta: text.to_owned(),
            },
            ActiveKind::Thinking => AssistantEvent::ThinkingDelta {
                block_id,
                delta: text.to_owned(),
            },
        })?;
        if let Some(signature) = signature.filter(|value| !value.is_empty()) {
            let replay_item = if let Some(item) = self
                .active
                .as_ref()
                .and_then(|active| active.replay_item.clone())
            {
                item
            } else {
                let active = self.active.as_ref().expect("active block");
                let item = self.start_replay(
                    ReplayTarget::ContentBlock(active.block_id.clone()),
                    provider_ordinal,
                )?;
                self.active.as_mut().expect("active block").replay_item = Some(item.clone());
                item
            };
            self.emit(AssistantEvent::ReplayData {
                item_id: replay_item,
                operation: ReplayDataOperation::ReplaceUtf8(signature.to_owned()),
            })?;
        }
        Ok(())
    }

    fn function_call(
        &mut self,
        function: &Map<String, Value>,
        signature: Option<&str>,
        provider_ordinal: u32,
    ) -> Result<(), GoogleDecodeError> {
        let name = optional_string(function, "name")?.unwrap_or_default();
        let provided = optional_string(function, "id")?.filter(|value| !value.is_empty());
        let mut call_id = provided.map(ToolCallId::new).unwrap_or_default();
        if call_id.as_str().is_empty() || self.used_tool_ids.contains(&call_id) {
            let sequence = next_tool_sequence(&self.context.api)?;
            call_id = ToolCallId::new(format!(
                "{name}_{}_{}",
                self.context.timestamp.unix_millis(),
                sequence
            ));
        }
        self.used_tool_ids.insert(call_id.clone());
        let block_id = ContentBlockId::new(format!(
            "google-block-{}-{}",
            self.context.message_id, self.next_content_index
        ));
        self.emit(AssistantEvent::ContentBlockStarted {
            block_id: block_id.clone(),
            content_index: self.next_content_index,
            kind: ContentBlockKind::ToolCall,
        })?;
        self.next_content_index = self.next_content_index.saturating_add(1);
        if let Some(signature) = signature.filter(|value| !value.is_empty()) {
            let replay_item =
                self.start_replay(ReplayTarget::ToolCall(call_id.clone()), provider_ordinal)?;
            self.emit(AssistantEvent::ReplayData {
                item_id: replay_item.clone(),
                operation: ReplayDataOperation::ReplaceUtf8(signature.to_owned()),
            })?;
            self.emit(AssistantEvent::ReplayItemFinished {
                item_id: replay_item,
            })?;
        }
        self.emit(AssistantEvent::ToolCallMetadata {
            block_id: block_id.clone(),
            call_id: call_id.clone(),
            name: Some(name.to_owned()),
        })?;
        let arguments = match function.get("args") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(arguments) => arguments.clone(),
        };
        self.emit(AssistantEvent::ToolArgumentsReplaced {
            block_id: block_id.clone(),
            arguments: serde_json::to_string(&arguments).map_err(|error| {
                GoogleDecodeError::new(format!("could not encode function arguments: {error}"))
            })?,
        })?;
        self.emit(AssistantEvent::ContentBlockFinished { block_id })?;
        self.saw_tool_call = true;
        Ok(())
    }

    fn start_replay(
        &mut self,
        target: ReplayTarget,
        ordinal: u32,
    ) -> Result<ReplayItemId, GoogleDecodeError> {
        let item_id = ReplayItemId::new(format!(
            "google-replay-{}-{ordinal}",
            self.context.message_id
        ));
        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: item_id.clone(),
            ordinal,
            target,
            kind: ReplayKind::new(GOOGLE_THOUGHT_SIGNATURE_KIND),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })?;
        Ok(item_id)
    }

    fn take_provider_ordinal(&mut self) -> u32 {
        let ordinal = self.next_provider_ordinal;
        self.next_provider_ordinal = self.next_provider_ordinal.saturating_add(1);
        ordinal
    }

    fn close_active(&mut self) -> Result<(), GoogleDecodeError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        if let Some(item_id) = active.replay_item {
            self.emit(AssistantEvent::ReplayItemFinished { item_id })?;
        }
        self.emit(AssistantEvent::ContentBlockFinished {
            block_id: active.block_id,
        })
    }

    fn update_usage(&mut self, usage: &Map<String, Value>) -> Result<(), GoogleDecodeError> {
        for field in [
            "promptTokenCount",
            "candidatesTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
            "totalTokenCount",
        ] {
            if usage
                .get(field)
                .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
            {
                return Err(GoogleDecodeError::new(format!(
                    "Google usage field {field} is not a non-negative integer"
                )));
            }
        }
        let cached = usage_u64(usage, "cachedContentTokenCount");
        let reasoning = usage_u64(usage, "thoughtsTokenCount");
        self.emit(AssistantEvent::UsageUpdated {
            cumulative: Usage {
                input_tokens: usage_u64(usage, "promptTokenCount").saturating_sub(cached),
                output_tokens: usage_u64(usage, "candidatesTokenCount").saturating_add(reasoning),
                reasoning_tokens: Some(reasoning),
                cache_read_tokens: Some(cached),
                cache_write_tokens: Some(0),
                cache_write_one_hour_tokens: None,
                total_tokens: usage.get("totalTokenCount").and_then(Value::as_u64),
                source: UsageSource::ProviderReported,
            },
        })
    }

    fn finish(&mut self) {
        if let Err(error) = self.close_active() {
            self.fail(error.to_string());
            return;
        }
        let Some(raw) = self.raw_finish_reason.clone() else {
            self.fail("Google stream ended without a finish reason".to_owned());
            return;
        };
        let reason = match raw.as_str() {
            "STOP" if self.saw_tool_call => AssistantFinishReason::ToolUse,
            "STOP" => AssistantFinishReason::Stop,
            "MAX_TOKENS" => AssistantFinishReason::Length,
            _ => {
                self.fail(format!("Provider stopped with: {raw}"));
                return;
            }
        };
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let fallback = assembler.clone();
        match assembler.finish_completed(AssistantFinish {
            reason,
            raw_provider_reason: Some(raw),
            error: None,
        }) {
            Ok(mut message) => match self.price_message(&mut message) {
                Ok(()) => self.events.push(AssistantEvent::Finished { message }),
                Err(error) => self.events.push(AssistantEvent::Failed {
                    message: fallback.finish_failed(
                        PublicError {
                            code: "provider_protocol".to_owned(),
                            message: error.to_string(),
                            retryable: false,
                            provider_code: None,
                            status: None,
                            request_id: self.response_id.clone(),
                        },
                        self.raw_finish_reason.clone(),
                    ),
                }),
            },
            Err(error) => self.events.push(AssistantEvent::Failed {
                message: fallback.finish_failed(
                    PublicError {
                        code: "provider_protocol".to_owned(),
                        message: format!("invalid completed Google stream: {error}"),
                        retryable: false,
                        provider_code: None,
                        status: None,
                        request_id: self.response_id.clone(),
                    },
                    self.raw_finish_reason.clone(),
                ),
            }),
        }
    }

    fn fail(&mut self, message: String) {
        self.fail_public(PublicError {
            code: "provider_protocol".to_owned(),
            message,
            retryable: false,
            provider_code: None,
            status: None,
            request_id: self.response_id.clone(),
        });
    }

    fn fail_public(&mut self, error: PublicError) {
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let mut message = assembler.finish_failed(error, self.raw_finish_reason.clone());
        let _ = self.price_message(&mut message);
        self.events.push(AssistantEvent::Failed { message });
    }

    fn cancel(&mut self, reason: CancellationReason) {
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let mut message = assembler.finish_cancelled(reason);
        let _ = self.price_message(&mut message);
        self.events.push(AssistantEvent::Cancelled { message });
    }

    fn price_message(
        &self,
        message: &mut pi_ai::AssistantMessage,
    ) -> Result<(), GoogleDecodeError> {
        message.cost = Some(
            self.context
                .pricing
                .calculate_cost(
                    &message.usage,
                    Currency::usd(),
                    CacheWriteRetention::Default,
                )
                .map_err(|error| {
                    GoogleDecodeError::new(format!(
                        "failed to calculate Google response cost: {error}"
                    ))
                })?,
        );
        Ok(())
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), GoogleDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| GoogleDecodeError::new("decoder already terminated"))?
            .apply(&event)
            .map_err(|error| GoogleDecodeError::new(format!("invalid decoded event: {error}")))?;
        self.events.push(event);
        Ok(())
    }
}

fn next_tool_sequence(api: &ApiId) -> Result<u64, GoogleDecodeError> {
    let counter = match api.as_str() {
        "google-generative-ai" => &GOOGLE_GENERATIVE_AI_TOOL_CALL_SEQUENCE,
        "google-vertex" => &GOOGLE_VERTEX_TOOL_CALL_SEQUENCE,
        other => {
            return Err(GoogleDecodeError::new(format!(
                "unsupported Google API family for tool-call identity: {other}"
            )));
        }
    };
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sequence| {
            sequence.checked_add(1)
        })
        .map(|sequence| sequence + 1)
        .map_err(|_| GoogleDecodeError::new("Google tool-call sequence exhausted"))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, GoogleDecodeError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(GoogleDecodeError::new(format!(
            "Google field {field} is not a string or null"
        ))),
    }
}

fn usage_u64(usage: &Map<String, Value>, field: &str) -> u64 {
    usage.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn find_line_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .map(|index| {
            let separator = if buffer[index] == b'\r' && buffer.get(index + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            (index, separator)
        })
}

fn sse_data(source: &str) -> Option<String> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let data = normalized
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>();
    (!data.is_empty()).then(|| data.join("\n"))
}
