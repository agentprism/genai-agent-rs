//! Anthropic Messages SSE decoding into replay-aware assistant events.

use agentprism_ai::{
    ANTHROPIC_REDACTED_THINKING_KIND, ANTHROPIC_THINKING_SIGNATURE_KIND, ApiId, AssistantAssembler,
    AssistantEvent, AssistantFinish, AssistantFinishReason, CancellationReason, ContentBlockId,
    ContentBlockKind, MessageId, ModelId, ProviderId, PublicError, ReplayApplicability,
    ReplayDataOperation, ReplayItemId, ReplayKind, ReplayTarget, Timestamp, ToolCallId, Usage,
    UsageSource,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

/// Stable identity inputs for one Anthropic decoder instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicMessagesDecodeContext {
    /// Stable canonical message identifier allocated before content events.
    pub message_id: MessageId,
    /// Provider serving the request.
    pub provider: ProviderId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Timestamp retained on the terminal assistant message.
    pub timestamp: Timestamp,
    /// Lowercased provider tool names mapped back to caller-defined names.
    pub tool_name_aliases: BTreeMap<String, String>,
}

/// A malformed or internally inconsistent Anthropic stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicMessagesDecodeError {
    message: String,
}

impl AnthropicMessagesDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AnthropicMessagesDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AnthropicMessagesDecodeError {}

/// Decodes a complete Anthropic SSE transcript and always returns a terminal
/// assistant event.
pub fn decode_anthropic_messages_sse(
    body: &[u8],
    context: AnthropicMessagesDecodeContext,
) -> Vec<AssistantEvent> {
    let mut decoder = AnthropicMessagesSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.push(body));
    events.extend(decoder.finish());
    events
}

/// Incremental Anthropic SSE decoder.
pub struct AnthropicMessagesSseDecoder {
    state: DecodeState,
    /// Bytes not yet terminated by CR, LF, or CRLF.
    buffer: Vec<u8>,
    /// Complete non-empty lines for the current SSE event, normalized to LF.
    record: Vec<u8>,
    initial_bom_pending: bool,
    terminated: bool,
}

impl AnthropicMessagesSseDecoder {
    /// Creates a decoder and emits its stable `MessageStarted` event.
    pub fn new(context: AnthropicMessagesDecodeContext) -> Self {
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

    /// Decodes every complete SSE record in one body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        if self.initial_bom_pending {
            const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
            if self.buffer.len() < UTF8_BOM.len() && UTF8_BOM.starts_with(&self.buffer) {
                return self.take_events();
            }
            if self.buffer.starts_with(UTF8_BOM) {
                self.buffer.drain(..UTF8_BOM.len());
            }
            self.initial_bom_pending = false;
        }
        while let Some((line_end, separator_length)) = find_line_boundary(&self.buffer) {
            let line = self.buffer[..line_end].to_vec();
            self.buffer.drain(..line_end + separator_length);
            if let Err(error) = self.consume_line(&line) {
                self.state.fail(error.to_string());
                self.terminated = true;
                break;
            }
        }
        self.take_events()
    }

    /// Completes body decoding and validates Anthropic's terminal markers.
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
    /// failed assistant message without making partial replay complete.
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

    /// Converts post-establishment cancellation into a committed partial
    /// assistant message.
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

    /// Returns whether this decoder already emitted a terminal event.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    fn consume_line(&mut self, line: &[u8]) -> Result<(), AnthropicMessagesDecodeError> {
        if line.is_empty() {
            return self.flush_record();
        }
        if !self.record.is_empty() {
            self.record.push(b'\n');
        }
        self.record.extend_from_slice(line);
        Ok(())
    }

    fn flush_record(&mut self) -> Result<(), AnthropicMessagesDecodeError> {
        if self.record.is_empty() {
            return Ok(());
        }
        let record = std::mem::take(&mut self.record);
        self.state.decode_sse_event(&record)
    }
}

#[derive(Clone)]
enum ActiveBlock {
    Text {
        block_id: ContentBlockId,
    },
    Thinking {
        block_id: ContentBlockId,
        replay_item: ReplayItemId,
    },
    ToolCall {
        block_id: ContentBlockId,
    },
}

impl ActiveBlock {
    fn block_id(&self) -> &ContentBlockId {
        match self {
            Self::Text { block_id }
            | Self::Thinking { block_id, .. }
            | Self::ToolCall { block_id } => block_id,
        }
    }
}

struct DecodeState {
    context: AnthropicMessagesDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    blocks: BTreeMap<u32, ActiveBlock>,
    next_content_index: u32,
    response_id: Option<String>,
    usage: Usage,
    saw_message_start: bool,
    saw_message_stop: bool,
    finish_reason: Option<AssistantFinishReason>,
    raw_finish_reason: Option<String>,
    finish_error: Option<String>,
}

impl DecodeState {
    fn new(context: AnthropicMessagesDecodeContext) -> Self {
        let mut state = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            events: Vec::new(),
            blocks: BTreeMap::new(),
            next_content_index: 0,
            response_id: None,
            usage: Usage::zero(UsageSource::Unknown),
            saw_message_start: false,
            saw_message_stop: false,
            finish_reason: None,
            raw_finish_reason: None,
            finish_error: None,
            context,
        };
        state
            .emit(AssistantEvent::MessageStarted {
                message_id: state.context.message_id.clone(),
                provider: state.context.provider.clone(),
                api: ApiId::new("anthropic-messages"),
                model: state.context.requested_model.clone(),
            })
            .expect("MessageStarted is valid on a fresh assembler");
        state
    }

    fn decode_sse_event(&mut self, record: &[u8]) -> Result<(), AnthropicMessagesDecodeError> {
        let source = std::str::from_utf8(record).map_err(|error| {
            AnthropicMessagesDecodeError::new(format!("SSE body is not UTF-8: {error}"))
        })?;
        let (event_name, data) = sse_fields(source);
        if event_name.as_deref() == Some("error") {
            return Err(AnthropicMessagesDecodeError::new(data.unwrap_or_default()));
        }
        let Some(data) = data else {
            return Ok(());
        };
        if !matches!(
            event_name.as_deref(),
            Some(
                "message_start"
                    | "message_delta"
                    | "message_stop"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
            )
        ) {
            return Ok(());
        }
        let event: Value = parse_json_with_repair(&data).map_err(|error| {
            AnthropicMessagesDecodeError::new(format!("invalid Anthropic SSE JSON: {error}"))
        })?;
        let event = event.as_object().ok_or_else(|| {
            AnthropicMessagesDecodeError::new("Anthropic SSE data is not an object")
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.message_start(event),
            Some("content_block_start") => self.content_block_start(event),
            Some("content_block_delta") => self.content_block_delta(event),
            Some("content_block_stop") => self.content_block_stop(event),
            Some("message_delta") => self.message_delta(event),
            Some("message_stop") => {
                self.saw_message_stop = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn message_start(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        self.saw_message_start = true;
        let message = required_object(event, "message", "message_start")?;
        let usage = required_object(message, "usage", "message_start.message")?;
        let response_id = optional_string(message, "id", "message_start.message")?
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let response_model = optional_string(message, "model", "message_start.message")?
            .filter(|model| !model.is_empty())
            .map(ModelId::new);
        if response_id.is_some() || response_model.is_some() {
            self.response_id.clone_from(&response_id);
            self.emit(AssistantEvent::ResponseMetadata {
                response_id,
                response_model,
                end_turn: None,
            })?;
        }
        self.replace_usage(usage)
    }

    fn content_block_start(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        let block = required_object(event, "content_block", "content_block_start")?;
        let block_type = required_string(block, "type", "content_block_start.content_block")?;
        if !matches!(
            block_type,
            "text" | "thinking" | "redacted_thinking" | "tool_use"
        ) {
            return Ok(());
        }
        let index = required_u32(event, "index", "content_block_start")?;
        let block_id = ContentBlockId::new(format!(
            "anthropic-block-{}-{index}",
            self.context.message_id
        ));
        let content_index = self.next_content_index;
        self.next_content_index = self.next_content_index.saturating_add(1);
        match block_type {
            "text" => {
                self.emit(AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index,
                    kind: ContentBlockKind::Text,
                })?;
                if let Some(text) =
                    optional_string(block, "text", "text content block")?.filter(|v| !v.is_empty())
                {
                    self.emit(AssistantEvent::TextDelta {
                        block_id: block_id.clone(),
                        delta: text.to_owned(),
                    })?;
                }
                self.blocks.insert(index, ActiveBlock::Text { block_id });
            }
            "thinking" => {
                self.emit(AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index,
                    kind: ContentBlockKind::Thinking,
                })?;
                if let Some(thinking) =
                    optional_string(block, "thinking", "thinking content block")?
                        .filter(|v| !v.is_empty())
                {
                    self.emit(AssistantEvent::ThinkingDelta {
                        block_id: block_id.clone(),
                        delta: thinking.to_owned(),
                    })?;
                }
                let replay_item = self.start_replay(
                    index,
                    block_id.clone(),
                    ANTHROPIC_THINKING_SIGNATURE_KIND,
                    ReplayDataOperation::AppendUtf8(
                        optional_string(block, "signature", "thinking content block")?
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                )?;
                self.blocks.insert(
                    index,
                    ActiveBlock::Thinking {
                        block_id,
                        replay_item,
                    },
                );
            }
            "redacted_thinking" => {
                self.emit(AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index,
                    kind: ContentBlockKind::Thinking,
                })?;
                self.emit(AssistantEvent::ThinkingDelta {
                    block_id: block_id.clone(),
                    delta: "[Reasoning redacted]".to_owned(),
                })?;
                let replay_item = self.start_replay(
                    index,
                    block_id.clone(),
                    ANTHROPIC_REDACTED_THINKING_KIND,
                    ReplayDataOperation::ReplaceUtf8(
                        required_string(block, "data", "redacted_thinking content block")?
                            .to_owned(),
                    ),
                )?;
                self.blocks.insert(
                    index,
                    ActiveBlock::Thinking {
                        block_id,
                        replay_item,
                    },
                );
            }
            "tool_use" => {
                self.emit(AssistantEvent::ContentBlockStarted {
                    block_id: block_id.clone(),
                    content_index,
                    kind: ContentBlockKind::ToolCall,
                })?;
                let provider_name = required_string(block, "name", "tool_use content block")?;
                let name = self
                    .context
                    .tool_name_aliases
                    .get(&provider_name.to_ascii_lowercase())
                    .map_or(provider_name, String::as_str)
                    .to_owned();
                self.emit(AssistantEvent::ToolCallMetadata {
                    block_id: block_id.clone(),
                    call_id: ToolCallId::new(required_string(
                        block,
                        "id",
                        "tool_use content block",
                    )?),
                    name: Some(name),
                })?;
                self.blocks
                    .insert(index, ActiveBlock::ToolCall { block_id });
            }
            _ => unreachable!("recognized Anthropic block type checked above"),
        }
        Ok(())
    }

    fn content_block_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        let delta = required_object(event, "delta", "content_block_delta")?;
        let delta_type = required_string(delta, "type", "content_block_delta.delta")?;
        if !matches!(
            delta_type,
            "text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta"
        ) {
            return Ok(());
        }
        let index = required_u32(event, "index", "content_block_delta")?;
        let Some(active) = self.blocks.get(&index).cloned() else {
            return Ok(());
        };
        match (delta_type, active) {
            ("text_delta", ActiveBlock::Text { block_id }) => {
                self.emit(AssistantEvent::TextDelta {
                    block_id,
                    delta: required_string(delta, "text", "text_delta")?.to_owned(),
                })
            }
            ("thinking_delta", ActiveBlock::Thinking { block_id, .. }) => {
                self.emit(AssistantEvent::ThinkingDelta {
                    block_id,
                    delta: required_string(delta, "thinking", "thinking_delta")?.to_owned(),
                })
            }
            ("signature_delta", ActiveBlock::Thinking { replay_item, .. }) => {
                self.emit(AssistantEvent::ReplayData {
                    item_id: replay_item,
                    operation: ReplayDataOperation::AppendUtf8(
                        required_string(delta, "signature", "signature_delta")?.to_owned(),
                    ),
                })
            }
            ("input_json_delta", ActiveBlock::ToolCall { block_id }) => {
                self.emit(AssistantEvent::ToolArgumentsDelta {
                    block_id,
                    delta: required_string(delta, "partial_json", "input_json_delta")?.to_owned(),
                })
            }
            _ => Ok(()),
        }
    }

    fn content_block_stop(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        let index = required_u32(event, "index", "content_block_stop")?;
        let Some(active) = self.blocks.remove(&index) else {
            return Ok(());
        };
        if let ActiveBlock::Thinking { replay_item, .. } = &active {
            self.emit(AssistantEvent::ReplayItemFinished {
                item_id: replay_item.clone(),
            })?;
        }
        self.emit(AssistantEvent::ContentBlockFinished {
            block_id: active.block_id().clone(),
        })
    }

    fn message_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        let delta = required_object(event, "delta", "message_delta")?;
        if let Some(reason) = optional_string(delta, "stop_reason", "message_delta.delta")? {
            self.raw_finish_reason = Some(reason.to_owned());
            let (finish, error) = map_stop_reason(reason, delta.get("stop_details"));
            self.finish_reason = Some(finish);
            self.finish_error = error;
        }
        if let Some(usage) = optional_object(event, "usage", "message_delta")? {
            self.update_present_usage(usage)?;
        }
        Ok(())
    }

    fn replace_usage(
        &mut self,
        usage: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        validate_usage(usage, "message_start.message.usage")?;
        self.usage = Usage {
            input_tokens: usage_u64(usage, "input_tokens"),
            output_tokens: usage_u64(usage, "output_tokens"),
            reasoning_tokens: usage
                .get("output_tokens_details")
                .and_then(Value::as_object)
                .and_then(|details| optional_usage_u64(details, "thinking_tokens")),
            cache_read_tokens: Some(usage_u64(usage, "cache_read_input_tokens")),
            cache_write_tokens: Some(usage_u64(usage, "cache_creation_input_tokens")),
            cache_write_one_hour_tokens: usage
                .get("cache_creation")
                .and_then(Value::as_object)
                .and_then(|creation| optional_usage_u64(creation, "ephemeral_1h_input_tokens")),
            total_tokens: None,
            source: UsageSource::ProviderReported,
        };
        self.emit(AssistantEvent::UsageUpdated {
            cumulative: self.usage.clone(),
        })
    }

    fn update_present_usage(
        &mut self,
        usage: &Map<String, Value>,
    ) -> Result<(), AnthropicMessagesDecodeError> {
        validate_usage(usage, "message_delta.usage")?;
        update_if_present(usage, "input_tokens", &mut self.usage.input_tokens);
        update_if_present(usage, "output_tokens", &mut self.usage.output_tokens);
        update_optional_if_present(
            usage,
            "cache_read_input_tokens",
            &mut self.usage.cache_read_tokens,
        );
        update_optional_if_present(
            usage,
            "cache_creation_input_tokens",
            &mut self.usage.cache_write_tokens,
        );
        if let Some(one_hour) = usage
            .get("cache_creation")
            .and_then(Value::as_object)
            .and_then(|creation| optional_usage_u64(creation, "ephemeral_1h_input_tokens"))
        {
            self.usage.cache_write_one_hour_tokens = Some(one_hour);
        }
        if let Some(thinking) = usage
            .get("output_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| optional_usage_u64(details, "thinking_tokens"))
        {
            self.usage.reasoning_tokens = Some(thinking);
        }
        self.usage.source = UsageSource::ProviderReported;
        self.emit(AssistantEvent::UsageUpdated {
            cumulative: self.usage.clone(),
        })
    }

    fn start_replay(
        &mut self,
        provider_index: u32,
        block_id: ContentBlockId,
        kind: &str,
        operation: ReplayDataOperation,
    ) -> Result<ReplayItemId, AnthropicMessagesDecodeError> {
        let item_id = ReplayItemId::new(format!(
            "anthropic-replay-{}-{provider_index}",
            self.context.message_id
        ));
        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: item_id.clone(),
            ordinal: provider_index,
            target: ReplayTarget::ContentBlock(block_id),
            kind: ReplayKind::new(kind),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })?;
        self.emit(AssistantEvent::ReplayData {
            item_id: item_id.clone(),
            operation,
        })?;
        Ok(item_id)
    }

    fn finish(&mut self) {
        if self.saw_message_start && !self.saw_message_stop {
            self.fail("Anthropic stream ended before message_stop".to_owned());
            return;
        }
        if self.finish_reason.is_none() {
            self.fail("Anthropic stream ended without a stop reason".to_owned());
            return;
        }
        if let Some(error) = self.finish_error.clone() {
            self.fail(error);
            return;
        }
        let finish = AssistantFinish {
            reason: self.finish_reason.expect("checked above"),
            raw_provider_reason: self.raw_finish_reason.clone(),
            error: None,
        };
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let fallback = assembler.clone();
        match assembler.finish_completed(finish) {
            Ok(message) => self.events.push(AssistantEvent::Finished { message }),
            Err(error) => self.events.push(AssistantEvent::Failed {
                message: fallback.finish_failed(
                    PublicError {
                        code: "provider_protocol".to_owned(),
                        message: format!("invalid completed Anthropic stream: {error}"),
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
        self.events.push(AssistantEvent::Failed {
            message: assembler.finish_failed(error, self.raw_finish_reason.clone()),
        });
    }

    fn cancel(&mut self, reason: CancellationReason) {
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        self.events.push(AssistantEvent::Cancelled {
            message: assembler.finish_cancelled(reason),
        });
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), AnthropicMessagesDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| AnthropicMessagesDecodeError::new("decoder already terminated"))?
            .apply(&event)
            .map_err(|error| {
                AnthropicMessagesDecodeError::new(format!("invalid decoded event: {error}"))
            })?;
        self.events.push(event);
        Ok(())
    }
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Map<String, Value>, AnthropicMessagesDecodeError> {
    object
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| malformed_field(context, field, "an object"))
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<&'a Map<String, Value>>, AnthropicMessagesDecodeError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(value)),
        Some(_) => Err(malformed_field(context, field, "an object or null")),
    }
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a str, AnthropicMessagesDecodeError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed_field(context, field, "a string"))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<&'a str>, AnthropicMessagesDecodeError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(malformed_field(context, field, "a string or null")),
    }
}

fn required_u32(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<u32, AnthropicMessagesDecodeError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| malformed_field(context, field, "a non-negative 32-bit integer"))
}

fn validate_usage(
    usage: &Map<String, Value>,
    context: &str,
) -> Result<(), AnthropicMessagesDecodeError> {
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        validate_optional_u64(usage, field, context)?;
    }
    if let Some(details) = optional_object(usage, "output_tokens_details", context)? {
        validate_optional_u64(details, "thinking_tokens", "output_tokens_details")?;
    }
    if let Some(creation) = optional_object(usage, "cache_creation", context)? {
        validate_optional_u64(creation, "ephemeral_1h_input_tokens", "cache_creation")?;
    }
    Ok(())
}

fn validate_optional_u64(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<(), AnthropicMessagesDecodeError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(()),
        Some(value) if value.as_u64().is_some() => Ok(()),
        Some(_) => Err(malformed_field(
            context,
            field,
            "a non-negative integer or null",
        )),
    }
}

fn malformed_field(context: &str, field: &str, expected: &str) -> AnthropicMessagesDecodeError {
    AnthropicMessagesDecodeError::new(format!(
        "malformed Anthropic {context}: required field `{field}` must be {expected}"
    ))
}

fn map_stop_reason(
    reason: &str,
    details: Option<&Value>,
) -> (AssistantFinishReason, Option<String>) {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (AssistantFinishReason::Stop, None),
        "max_tokens" => (AssistantFinishReason::Length, None),
        "tool_use" => (AssistantFinishReason::ToolUse, None),
        "refusal" => (
            AssistantFinishReason::Error,
            Some(
                details
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("The model refused to complete the request")
                    .to_owned(),
            ),
        ),
        "sensitive" => (
            AssistantFinishReason::Error,
            Some("Provider stopped with: sensitive".to_owned()),
        ),
        other => (
            AssistantFinishReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}

fn usage_u64(usage: &Map<String, Value>, name: &str) -> u64 {
    usage.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn optional_usage_u64(usage: &Map<String, Value>, name: &str) -> Option<u64> {
    usage
        .get(name)
        .filter(|value| !value.is_null())
        .and_then(Value::as_u64)
}

fn update_if_present(usage: &Map<String, Value>, name: &str, target: &mut u64) {
    if let Some(value) = optional_usage_u64(usage, name) {
        *target = value;
    }
}

fn update_optional_if_present(usage: &Map<String, Value>, name: &str, target: &mut Option<u64>) {
    if let Some(value) = optional_usage_u64(usage, name) {
        *target = Some(value);
    }
}

fn find_line_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .map(|index| {
            let separator_length =
                if buffer[index] == b'\r' && buffer.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            (index, separator_length)
        })
}

fn sse_fields(source: &str) -> (Option<String>, Option<String>) {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut event = None;
    let mut data = Vec::new();
    for line in normalized.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    (event, (!data.is_empty()).then(|| data.join("\n")))
}

fn parse_json_with_repair(input: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(input).or_else(|original| {
        let repaired = repair_json(input);
        if repaired == input {
            Err(original)
        } else {
            serde_json::from_str(&repaired)
        }
    })
}

fn repair_json(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut repaired = String::with_capacity(input.len());
    let mut in_string = false;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if !in_string {
            repaired.push(character);
            if character == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            repaired.push(character);
            in_string = false;
            index += 1;
            continue;
        }
        if character == '\\' {
            let Some(next) = characters.get(index + 1).copied() else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };
            if next == 'u'
                && characters.get(index + 2..index + 6).is_some_and(|digits| {
                    digits.len() == 4 && digits.iter().all(|digit| digit.is_ascii_hexdigit())
                })
            {
                repaired.extend(characters[index..index + 6].iter());
                index += 6;
                continue;
            }
            if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }
            repaired.push_str("\\\\");
            index += 1;
            continue;
        }
        match character {
            '\u{0008}' => repaired.push_str("\\b"),
            '\u{000c}' => repaired.push_str("\\f"),
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            control if control <= '\u{001f}' => {
                use std::fmt::Write as _;
                write!(repaired, "\\u{:04x}", u32::from(control))
                    .expect("writing to String cannot fail");
            }
            _ => repaired.push(character),
        }
        index += 1;
    }
    repaired
}
