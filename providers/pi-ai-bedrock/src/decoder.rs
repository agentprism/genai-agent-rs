//! AWS event-stream decoding for Bedrock Converse Stream.

use base64::Engine as _;
use http::HeaderMap;
use pi_ai::{
    ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION, ApiId, AssistantAssembler, AssistantEvent,
    AssistantFinish, AssistantFinishReason, AssistantMessage, AssistantMessageDiagnostic,
    BEDROCK_REDACTED_REASONING_KIND, BEDROCK_THINKING_SIGNATURE_KIND, CacheWriteRetention,
    CancellationReason, ContentBlockId, ContentBlockKind, Currency, MessageId, ModelId,
    ModelPricing, ProviderId, PublicError, ReplayApplicability, ReplayDataOperation, ReplayItemId,
    ReplayKind, ReplayTarget, Timestamp, ToolCallId, TransportError, Usage, UsageSource,
    trim_ecmascript,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS: usize = 200;

/// Stable identity and pricing inputs for one Bedrock decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockDecodeContext {
    /// Stable canonical message identifier.
    pub message_id: MessageId,
    /// Provider serving the request.
    pub provider: ProviderId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Requested-model pricing.
    pub pricing: ModelPricing,
    /// Timestamp retained by terminal assembly.
    pub timestamp: Timestamp,
}

/// A malformed or inconsistent Bedrock stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockDecodeError {
    message: String,
    provider_code: Option<String>,
}

impl BedrockDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provider_code: None,
        }
    }

    fn provider(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provider_code: Some(code.into()),
        }
    }
}

impl fmt::Display for BedrockDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BedrockDecodeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

#[derive(Clone, Debug)]
struct Block {
    id: ContentBlockId,
    kind: BlockKind,
    replay: Option<ReplayItemId>,
    redacted: bool,
}

/// Incremental decoder for AWS's framed event-stream response body.
pub struct BedrockConverseDecoder {
    context: BedrockDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    frames: Vec<u8>,
    blocks: BTreeMap<u32, Block>,
    next_content_index: u32,
    next_replay_ordinal: u32,
    stop_reason: Option<String>,
    response_status: Option<u16>,
    response_request_id: Option<String>,
    terminated: bool,
}

impl BedrockConverseDecoder {
    /// Creates a decoder and queues the canonical `MessageStarted` event.
    pub fn new(context: BedrockDecodeContext) -> Self {
        let mut decoder = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            context,
            events: Vec::new(),
            frames: Vec::new(),
            blocks: BTreeMap::new(),
            next_content_index: 0,
            next_replay_ordinal: 0,
            stop_reason: None,
            response_status: None,
            response_request_id: None,
            terminated: false,
        };
        decoder
            .emit(AssistantEvent::MessageStarted {
                message_id: decoder.context.message_id.clone(),
                provider: decoder.context.provider.clone(),
                api: ApiId::new("bedrock-converse-stream"),
                model: decoder.context.requested_model.clone(),
            })
            .expect("a fresh assembler accepts MessageStarted");
        decoder
    }

    /// Drains normalized events queued since the preceding call.
    pub fn take_events(&mut self) -> Vec<AssistantEvent> {
        std::mem::take(&mut self.events)
    }

    /// Captures raw Smithy response metadata before the body is consumed.
    pub fn observe_response(&mut self, status: u16, headers: &HeaderMap) {
        self.response_status = Some(status);
        self.response_request_id = ["x-amzn-requestid", "x-amzn-request-id"]
            .into_iter()
            .find_map(|name| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .and_then(normalize_diagnostic_value)
            });
    }

    /// Seeds a redacted transport diagnostic captured before stream decoding.
    pub fn add_diagnostic(&mut self, diagnostic: AssistantMessageDiagnostic) {
        if !self.terminated {
            self.emit(AssistantEvent::DiagnosticAdded { diagnostic })
                .expect("an active decoder accepts response diagnostics");
        }
    }

    /// Decodes complete AWS event-stream frames from a response-body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.frames.extend_from_slice(chunk);
        while let Some(total) = self
            .frames
            .get(..4)
            .map(|bytes| u32::from_be_bytes(bytes.try_into().expect("four bytes")) as usize)
        {
            if total < 16 {
                self.fail("invalid AWS event-stream frame length".to_owned());
                break;
            }
            if self.frames.len() < total {
                break;
            }
            let frame = self.frames.drain(..total).collect::<Vec<_>>();
            if let Err(error) = self.decode_frame(&frame) {
                self.fail_decode(error);
                break;
            }
        }
        self.take_events()
    }

    /// Decodes one modeled Converse Stream event. This is also the hermetic
    /// conformance seam used by captured Smithy fixtures.
    pub fn push_event(&mut self, event_type: &str, event: &Value) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        if let Err(error) = self.decode_event(event_type, event) {
            self.fail_decode(error);
        }
        self.take_events()
    }

    /// Completes stream decoding and emits exactly one terminal event.
    pub fn finish(&mut self) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        if !self.frames.is_empty() {
            self.fail("Bedrock stream ended with a partial AWS event-stream frame".to_owned());
            return self.take_events();
        }
        if let Err(error) = self.finish_success() {
            self.fail_decode(error);
        }
        self.terminated = true;
        self.take_events()
    }

    /// Converts a post-establishment transport failure into a committed failure.
    pub fn fail_transport(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<AssistantEvent> {
        self.fail_transport_error(TransportError::new(code, message))
    }

    /// Converts an enriched SDK/transport failure into a committed failure.
    pub fn fail_transport_error(&mut self, error: TransportError) -> Vec<AssistantEvent> {
        if !self.terminated {
            if let Some(status) = error.status {
                self.response_status = Some(status);
            }
            if error.request_id.is_some() {
                self.response_request_id.clone_from(&error.request_id);
            }
            let provider_code = error.provider_code.or_else(|| {
                error
                    .code
                    .ends_with("Exception")
                    .then(|| error.code.clone())
            });
            self.fail_public(PublicError {
                code: error.code,
                message: error.message,
                retryable: false,
                provider_code,
                status: error.status,
                request_id: self.response_request_id.clone(),
            });
        }
        self.take_events()
    }

    /// Converts cancellation into a committed partial message.
    pub fn cancel(&mut self, message: impl Into<String>) -> Vec<AssistantEvent> {
        if !self.terminated {
            if let Some(assembler) = self.assembler.take() {
                let reason = match &self.response_request_id {
                    Some(request_id) => {
                        CancellationReason::new(message).with_request_id(request_id.clone())
                    }
                    None => CancellationReason::new(message),
                };
                let mut message = assembler.finish_cancelled(reason);
                self.calculate_message_cost(&mut message);
                self.events.push(AssistantEvent::Cancelled { message });
            }
            self.terminated = true;
        }
        self.take_events()
    }

    /// Returns whether a terminal event has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    fn decode_frame(&mut self, frame: &[u8]) -> Result<(), BedrockDecodeError> {
        let total = read_u32(frame, 0)? as usize;
        let headers_len = read_u32(frame, 4)? as usize;
        if total != frame.len() || 12 + headers_len + 4 > total {
            return Err(BedrockDecodeError::new(
                "invalid AWS event-stream frame bounds",
            ));
        }
        if crc32(&frame[..8]) != read_u32(frame, 8)? {
            return Err(BedrockDecodeError::new(
                "invalid AWS event-stream prelude CRC",
            ));
        }
        if crc32(&frame[..total - 4]) != read_u32(frame, total - 4)? {
            return Err(BedrockDecodeError::new(
                "invalid AWS event-stream message CRC",
            ));
        }
        let headers = parse_headers(&frame[12..12 + headers_len])?;
        let payload = &frame[12 + headers_len..total - 4];
        let message_type = headers.get(":message-type").map(String::as_str);
        if message_type == Some("exception") || message_type == Some("error") {
            let kind = headers
                .get(":exception-type")
                .or_else(|| headers.get(":error-code"))
                .map_or("BedrockException", String::as_str);
            let value = serde_json::from_slice::<Value>(payload).ok();
            let detail = value
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Bedrock stream exception");
            if message_type == Some("error") {
                return Err(BedrockDecodeError::provider(detail, kind));
            }
            // Smithy exposes modeled event-stream exceptions as the thrown
            // event object. Pi's non-Error normalization JSON-stringifies that
            // object and does not prefix it with the modeled event name.
            let message = value
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok())
                .unwrap_or_else(|| detail.to_owned());
            return Err(BedrockDecodeError::new(message));
        }
        let event_type = headers
            .get(":event-type")
            .ok_or_else(|| BedrockDecodeError::new("AWS event-stream frame omits :event-type"))?;
        let value = serde_json::from_slice(payload).map_err(|error| {
            BedrockDecodeError::new(format!("invalid Bedrock event JSON: {error}"))
        })?;
        self.decode_event(event_type, &value)
    }

    fn decode_event(&mut self, event_type: &str, event: &Value) -> Result<(), BedrockDecodeError> {
        if !matches!(
            event_type,
            "messageStart"
                | "contentBlockStart"
                | "contentBlockDelta"
                | "contentBlockStop"
                | "messageStop"
                | "metadata"
                | "internalServerException"
                | "modelStreamErrorException"
                | "validationException"
                | "throttlingException"
                | "serviceUnavailableException"
        ) {
            // Pinned Pi has no final `else` in its top-level event chain.
            return Ok(());
        }
        let event = event
            .as_object()
            .ok_or_else(|| BedrockDecodeError::new("Bedrock event is not an object"))?;
        match event_type {
            "messageStart" => {
                if event.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err(BedrockDecodeError::new(
                        "unexpected Bedrock non-assistant message start",
                    ));
                }
            }
            "contentBlockStart" => self.start_content(event)?,
            "contentBlockDelta" => self.delta_content(event)?,
            "contentBlockStop" => {
                if let Some(index) = event_u32(event, "contentBlockIndex") {
                    self.finish_block(index)?;
                }
            }
            "messageStop" => {
                self.stop_reason = Some(required_string(event, "stopReason")?.to_owned());
            }
            "metadata" => self.usage(event)?,
            "internalServerException"
            | "modelStreamErrorException"
            | "validationException"
            | "throttlingException"
            | "serviceUnavailableException" => {
                // The AWS SDK yields these modeled members as plain objects.
                // Pi throws the member and its normalizer JSON-stringifies it;
                // the union discriminant is not part of the public message.
                return Err(BedrockDecodeError::new(
                    serde_json::to_string(event)
                        .unwrap_or_else(|_| "Bedrock stream exception".to_owned()),
                ));
            }
            _ => unreachable!("unknown event types return before object decoding"),
        }
        Ok(())
    }

    fn start_content(&mut self, event: &Map<String, Value>) -> Result<(), BedrockDecodeError> {
        let Some(tool) = event
            .get("start")
            .and_then(Value::as_object)
            .and_then(|start| start.get("toolUse"))
            .and_then(Value::as_object)
        else {
            return Ok(());
        };
        let index = required_u32(event, "contentBlockIndex")?;
        let id = self.start_block(index, BlockKind::Tool)?;
        self.emit(AssistantEvent::ToolCallMetadata {
            block_id: id,
            call_id: ToolCallId::new(tool.get("toolUseId").and_then(Value::as_str).unwrap_or("")),
            name: Some(
                tool.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
        })?;
        Ok(())
    }

    fn delta_content(&mut self, event: &Map<String, Value>) -> Result<(), BedrockDecodeError> {
        let index = required_u32(event, "contentBlockIndex")?;
        let Some(delta) = event.get("delta").and_then(Value::as_object) else {
            return Ok(());
        };
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            let id = match self.blocks.get(&index) {
                Some(block) if block.kind == BlockKind::Text => block.id.clone(),
                Some(_) => return Ok(()),
                None => self.start_block(index, BlockKind::Text)?,
            };
            return self.emit(AssistantEvent::TextDelta {
                block_id: id,
                delta: text.to_owned(),
            });
        }
        if let Some(tool) = delta.get("toolUse").and_then(Value::as_object) {
            let Some(id) = self
                .blocks
                .get(&index)
                .filter(|block| block.kind == BlockKind::Tool)
                .map(|block| block.id.clone())
            else {
                // Unlike text and reasoning, a tool delta never creates its
                // block; Bedrock must have sent the preceding tool-use start.
                return Ok(());
            };
            let fragment = tool.get("input").and_then(Value::as_str).unwrap_or("");
            return self.emit(AssistantEvent::ToolArgumentsDelta {
                block_id: id,
                delta: fragment.to_owned(),
            });
        }
        if let Some(reasoning) = delta.get("reasoningContent").and_then(Value::as_object) {
            let id = match self.blocks.get(&index) {
                Some(block) if block.kind == BlockKind::Thinking => block.id.clone(),
                Some(_) => return Ok(()),
                None => self.start_block(index, BlockKind::Thinking)?,
            };
            if let Some(text) = reasoning
                .get("text")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                self.emit(AssistantEvent::ThinkingDelta {
                    block_id: id.clone(),
                    delta: text.to_owned(),
                })?;
            }
            if let Some(signature) = reasoning
                .get("signature")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                && !self.blocks.get(&index).is_some_and(|block| block.redacted)
            {
                let replay = self.ensure_replay(index, BEDROCK_THINKING_SIGNATURE_KIND)?;
                self.emit(AssistantEvent::ReplayData {
                    item_id: replay,
                    operation: ReplayDataOperation::AppendUtf8(signature.to_owned()),
                })?;
            }
            if let Some(redacted) = reasoning.get("redactedContent") {
                let bytes = decode_bytes(redacted)?;
                if !bytes.is_empty() {
                    let first = !self.blocks.get(&index).is_some_and(|block| block.redacted);
                    if first {
                        if let Some(item_id) = self
                            .blocks
                            .get(&index)
                            .and_then(|block| block.replay.clone())
                        {
                            self.emit(AssistantEvent::ReplayItemDiscarded { item_id })?;
                        }
                        if let Some(block) = self.blocks.get_mut(&index) {
                            block.redacted = true;
                            block.replay = None;
                        }
                        self.emit(AssistantEvent::ThinkingDelta {
                            block_id: id,
                            delta: REDACTED_THINKING_PLACEHOLDER.to_owned(),
                        })?;
                    }
                    let replay = self.ensure_replay(index, BEDROCK_REDACTED_REASONING_KIND)?;
                    self.emit(AssistantEvent::ReplayData {
                        item_id: replay,
                        operation: ReplayDataOperation::AppendBytes(bytes),
                    })?;
                }
            }
            return Ok(());
        }
        Ok(())
    }

    fn start_block(
        &mut self,
        provider_index: u32,
        kind: BlockKind,
    ) -> Result<ContentBlockId, BedrockDecodeError> {
        if self.blocks.contains_key(&provider_index) {
            return Err(BedrockDecodeError::new("duplicate Bedrock content block"));
        }
        let id = ContentBlockId::new(format!(
            "bedrock-block-{}-{provider_index}",
            self.context.message_id
        ));
        let content_kind = match kind {
            BlockKind::Text => ContentBlockKind::Text,
            BlockKind::Thinking => ContentBlockKind::Thinking,
            BlockKind::Tool => ContentBlockKind::ToolCall,
        };
        self.emit(AssistantEvent::ContentBlockStarted {
            block_id: id.clone(),
            content_index: self.next_content_index,
            kind: content_kind,
        })?;
        self.next_content_index = self.next_content_index.saturating_add(1);
        self.blocks.insert(
            provider_index,
            Block {
                id: id.clone(),
                kind,
                replay: None,
                redacted: false,
            },
        );
        Ok(id)
    }

    fn ensure_replay(
        &mut self,
        provider_index: u32,
        kind: &str,
    ) -> Result<ReplayItemId, BedrockDecodeError> {
        if let Some(item) = self
            .blocks
            .get(&provider_index)
            .and_then(|block| block.replay.clone())
        {
            return Ok(item);
        }
        let block_id = self
            .blocks
            .get(&provider_index)
            .ok_or_else(|| BedrockDecodeError::new("replay targets an unknown block"))?
            .id
            .clone();
        let item_id = ReplayItemId::new(format!(
            "bedrock-replay-{}-{}",
            self.context.message_id, self.next_replay_ordinal
        ));
        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: item_id.clone(),
            ordinal: self.next_replay_ordinal,
            target: ReplayTarget::ContentBlock(block_id),
            kind: ReplayKind::new(kind),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })?;
        self.next_replay_ordinal = self.next_replay_ordinal.saturating_add(1);
        self.blocks
            .get_mut(&provider_index)
            .expect("block checked above")
            .replay = Some(item_id.clone());
        Ok(item_id)
    }

    fn finish_block(&mut self, provider_index: u32) -> Result<(), BedrockDecodeError> {
        let Some(block) = self.blocks.remove(&provider_index) else {
            return Ok(());
        };
        if let Some(item_id) = block.replay {
            self.emit(AssistantEvent::ReplayItemFinished { item_id })?;
        }
        self.emit(AssistantEvent::ContentBlockFinished { block_id: block.id })
    }

    fn usage(&mut self, event: &Map<String, Value>) -> Result<(), BedrockDecodeError> {
        let Some(usage) = event.get("usage").and_then(Value::as_object) else {
            return Ok(());
        };
        let input = optional_u64(usage, "inputTokens")?;
        let output = optional_u64(usage, "outputTokens")?;
        let cache_read = optional_u64(usage, "cacheReadInputTokens")?;
        let cache_write = optional_u64(usage, "cacheWriteInputTokens")?;
        self.emit(AssistantEvent::UsageUpdated {
            cumulative: Usage {
                input_tokens: input,
                output_tokens: output,
                reasoning_tokens: None,
                cache_read_tokens: Some(cache_read),
                cache_write_tokens: Some(cache_write),
                cache_write_one_hour_tokens: None,
                total_tokens: event
                    .get("usage")
                    .and_then(Value::as_object)
                    .and_then(|value| value.get("totalTokens"))
                    .and_then(Value::as_u64)
                    .filter(|value| *value != 0)
                    .or(Some(input.saturating_add(output))),
                source: UsageSource::ProviderReported,
            },
        })
    }

    fn finish_success(&mut self) -> Result<(), BedrockDecodeError> {
        let raw = self
            .stop_reason
            .clone()
            .ok_or_else(|| BedrockDecodeError::new("Bedrock stream ended without a stop reason"))?;
        for index in self.blocks.keys().copied().collect::<Vec<_>>() {
            self.finish_block(index)?;
        }
        let reason = match raw.as_str() {
            "end_turn" | "stop_sequence" => AssistantFinishReason::Stop,
            "tool_use" => AssistantFinishReason::ToolUse,
            "max_tokens" | "model_context_window_exceeded" => AssistantFinishReason::Length,
            other => {
                return Err(BedrockDecodeError::new(format!(
                    "Provider stopped with: {other}"
                )));
            }
        };
        let assembler = self
            .assembler
            .take()
            .ok_or_else(|| BedrockDecodeError::new("decoder already terminated"))?;
        let fallback = assembler.clone();
        match assembler.finish_completed(AssistantFinish {
            reason,
            raw_provider_reason: Some(raw.clone()),
            error: None,
        }) {
            Ok(mut message) => {
                message.cost = Some(self.message_cost(&message)?);
                self.events.push(AssistantEvent::Finished { message });
            }
            Err(error) => {
                let mut message = fallback.finish_failed(
                    PublicError {
                        code: "provider_protocol".to_owned(),
                        message: format!("invalid completed Bedrock stream: {error}"),
                        retryable: false,
                        provider_code: None,
                        status: None,
                        request_id: self.response_request_id.clone(),
                    },
                    Some(raw),
                );
                self.calculate_message_cost(&mut message);
                self.events.push(AssistantEvent::Failed { message });
            }
        }
        Ok(())
    }

    fn fail(&mut self, message: String) {
        self.fail_public(PublicError {
            code: "provider_protocol".to_owned(),
            message,
            retryable: false,
            provider_code: None,
            status: None,
            request_id: self.response_request_id.clone(),
        });
    }

    fn fail_decode(&mut self, error: BedrockDecodeError) {
        self.fail_public(PublicError {
            code: "provider_protocol".to_owned(),
            message: error.message,
            retryable: false,
            provider_code: error.provider_code,
            status: None,
            request_id: self.response_request_id.clone(),
        });
    }

    fn fail_public(&mut self, mut error: PublicError) {
        if error.request_id.is_none() {
            error.request_id.clone_from(&self.response_request_id);
        }
        self.add_failure_diagnostic(&error);
        if let Some(assembler) = self.assembler.take() {
            let mut message = assembler.finish_failed(error, self.stop_reason.clone());
            self.calculate_message_cost(&mut message);
            self.events.push(AssistantEvent::Failed { message });
        }
        self.terminated = true;
    }

    fn add_failure_diagnostic(&mut self, error: &PublicError) {
        let mut details = BTreeMap::new();
        if let Some(status) = self
            .response_status
            .filter(|status| !(200..300).contains(status))
        {
            details.insert("status".to_owned(), Value::from(status));
        }
        if let Some(code) = error
            .provider_code
            .as_deref()
            .filter(|code| code.ends_with("Exception"))
            .and_then(normalize_diagnostic_value)
        {
            details.insert("errorCode".to_owned(), Value::String(code));
        }
        if let Some(request_id) = error
            .request_id
            .as_deref()
            .and_then(normalize_diagnostic_value)
        {
            details.insert("requestId".to_owned(), Value::String(request_id));
        }
        if !details.is_empty() {
            let diagnostic = AssistantMessageDiagnostic {
                schema_version: ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
                kind: "bedrock_response_failure".to_owned(),
                timestamp: self.context.timestamp,
                error: None,
                details,
            };
            self.emit(AssistantEvent::DiagnosticAdded { diagnostic })
                .expect("an active decoder accepts Bedrock failure diagnostics");
        }
    }

    fn message_cost(&self, message: &AssistantMessage) -> Result<pi_ai::Cost, BedrockDecodeError> {
        self.context
            .pricing
            .calculate_cost(
                &message.usage,
                Currency::usd(),
                CacheWriteRetention::Default,
            )
            .map_err(|error| {
                BedrockDecodeError::new(format!("failed to price Bedrock usage: {error}"))
            })
    }

    fn calculate_message_cost(&self, message: &mut AssistantMessage) {
        message.cost = self.message_cost(message).ok();
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), BedrockDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| BedrockDecodeError::new("decoder already terminated"))?
            .apply(&event)
            .map_err(|error| BedrockDecodeError::new(format!("invalid decoded event: {error}")))?;
        self.events.push(event);
        Ok(())
    }
}

fn normalize_diagnostic_value(value: &str) -> Option<String> {
    let trimmed = trim_ecmascript(value);
    let js_character_count = trimmed.encode_utf16().count();
    (!trimmed.is_empty() && js_character_count <= MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS)
        .then(|| trimmed.to_owned())
}

fn decode_bytes(value: &Value) -> Result<Vec<u8>, BedrockDecodeError> {
    if let Some(encoded) = value.as_str() {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| BedrockDecodeError::new(format!("invalid redactedContent: {error}")));
    }
    value
        .as_array()
        .ok_or_else(|| BedrockDecodeError::new("redactedContent is not bytes"))?
        .iter()
        .map(|byte| {
            byte.as_u64()
                .and_then(|byte| u8::try_from(byte).ok())
                .ok_or_else(|| BedrockDecodeError::new("redactedContent contains a non-byte"))
        })
        .collect()
}

fn parse_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, BedrockDecodeError> {
    let mut result = BTreeMap::new();
    let mut index = 0;
    while index < bytes.len() {
        let name_len = usize::from(
            *bytes
                .get(index)
                .ok_or_else(|| BedrockDecodeError::new("truncated event-stream header"))?,
        );
        index += 1;
        let name_end = index.saturating_add(name_len);
        let name = std::str::from_utf8(
            bytes
                .get(index..name_end)
                .ok_or_else(|| BedrockDecodeError::new("truncated event-stream header name"))?,
        )
        .map_err(|_| BedrockDecodeError::new("event-stream header name is not UTF-8"))?;
        index = name_end;
        let kind = *bytes
            .get(index)
            .ok_or_else(|| BedrockDecodeError::new("truncated event-stream header type"))?;
        index += 1;
        if kind != 7 {
            return Err(BedrockDecodeError::new(
                "unsupported event-stream header type",
            ));
        }
        let length = usize::from(u16::from_be_bytes(
            bytes
                .get(index..index + 2)
                .ok_or_else(|| BedrockDecodeError::new("truncated event-stream string length"))?
                .try_into()
                .expect("two bytes"),
        ));
        index += 2;
        let end = index.saturating_add(length);
        let value = std::str::from_utf8(
            bytes
                .get(index..end)
                .ok_or_else(|| BedrockDecodeError::new("truncated event-stream string"))?,
        )
        .map_err(|_| BedrockDecodeError::new("event-stream header value is not UTF-8"))?;
        result.insert(name.to_owned(), value.to_owned());
        index = end;
    }
    Ok(result)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BedrockDecodeError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| BedrockDecodeError::new("truncated AWS event-stream frame"))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn required_u32(object: &Map<String, Value>, field: &str) -> Result<u32, BedrockDecodeError> {
    event_u32(object, field).ok_or_else(|| BedrockDecodeError::new(format!("{field} is not a u32")))
}

fn event_u32(object: &Map<String, Value>, field: &str) -> Option<u32> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, BedrockDecodeError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| BedrockDecodeError::new(format!("{field} is not a string")))
}

fn optional_u64(object: &Map<String, Value>, field: &str) -> Result<u64, BedrockDecodeError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or_else(|| {
            BedrockDecodeError::new(format!("{field} is not a non-negative integer"))
        }),
    }
}
