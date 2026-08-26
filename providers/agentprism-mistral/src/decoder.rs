//! Mistral Conversations SSE decoding into replay-aware assistant events.

use agentprism_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    AssistantMessageDiagnostic, CancellationReason, ContentBlockId, ContentBlockKind, MessageId,
    ModelId, OPENAI_CHAT_REASONING_DETAIL_KIND, OPENAI_CHAT_REASONING_FIELD_KIND, OrderedJsonValue,
    OrderedJsonWriter, ProviderId, PublicError, ReplayApplicability, ReplayDataOperation,
    ReplayItemId, ReplayKind, ReplayTarget, Timestamp, ToolCallId, TransportError, Usage,
    UsageSource, trim_ecmascript, trim_start_ecmascript,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// Stable identity and compatibility inputs for one decoder instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MistralConversationsDecodeContext {
    /// Stable canonical message identifier allocated before content events.
    pub message_id: MessageId,
    /// Provider serving the request.
    pub provider: ProviderId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Timestamp retained on the terminal assistant message.
    pub timestamp: Timestamp,
    /// Whether absence of a provider `finish_reason` is an error.
    pub supports_finish_reason: bool,
    /// Grammar-tool name to canonical string-argument property.
    pub grammar_tool_input_properties: BTreeMap<String, String>,
}

/// A malformed or internally inconsistent Mistral Conversations stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MistralConversationsDecodeError {
    message: String,
}

impl MistralConversationsDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MistralConversationsDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MistralConversationsDecodeError {}

/// Decodes a complete SSE transcript and always returns a terminal event.
///
/// Provider protocol failures become a committed [`AssistantEvent::Failed`]
/// record after every successfully decoded partial event, matching Pi's
/// stream-error commitment behavior.
pub fn decode_mistral_conversations_sse(
    body: &[u8],
    context: MistralConversationsDecodeContext,
) -> Vec<AssistantEvent> {
    let mut decoder = MistralConversationsSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.push(body));
    events.extend(decoder.finish());
    events
}

/// Creates a started-and-failed event sequence for body-transport failures.
pub fn failed_mistral_conversations_events(
    context: MistralConversationsDecodeContext,
    code: impl Into<String>,
    message: impl Into<String>,
) -> Vec<AssistantEvent> {
    let mut decoder = MistralConversationsSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.fail_transport(code, message));
    events
}

/// Incremental SSE decoder retaining canonical partial state across body
/// chunks, transport failures, and cancellation after response establishment.
pub struct MistralConversationsSseDecoder {
    state: DecodeState,
    buffer: Vec<u8>,
    terminated: bool,
}

impl MistralConversationsSseDecoder {
    /// Creates a decoder and assembles its stable `MessageStarted` event.
    pub fn new(context: MistralConversationsDecodeContext) -> Self {
        Self {
            state: DecodeState::new(context),
            buffer: Vec::new(),
            terminated: false,
        }
    }

    /// Drains events produced since the preceding call.
    pub fn take_events(&mut self) -> Vec<AssistantEvent> {
        std::mem::take(&mut self.state.events)
    }

    /// Seeds a redacted transport recovery diagnostic before provider body
    /// events are consumed.
    pub fn add_diagnostic(&mut self, diagnostic: AssistantMessageDiagnostic) {
        if !self.terminated {
            self.state
                .emit(AssistantEvent::DiagnosticAdded { diagnostic })
                .expect("an active Mistral decoder accepts response diagnostics");
        }
    }

    /// Decodes every complete SSE event available in one body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        while let Some((boundary, separator_length)) = find_sse_boundary(&self.buffer) {
            let event = self.buffer[..boundary].to_vec();
            self.buffer.drain(..boundary + separator_length);
            if let Err(error) = self.state.decode_sse_event(&event) {
                self.state.fail(error.to_string());
                self.terminated = true;
                break;
            }
        }
        self.take_events()
    }

    /// Finishes at body EOF, including one final event without a blank-line
    /// delimiter, then validates the provider terminal reason.
    pub fn finish(&mut self) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        let has_data = std::str::from_utf8(&self.buffer)
            .map_or(true, |value| !trim_ecmascript(value).is_empty());
        if has_data {
            let event = std::mem::take(&mut self.buffer);
            if let Err(error) = self.state.decode_sse_event(&event) {
                self.state.fail(error.to_string());
                self.terminated = true;
                return self.take_events();
            }
        }
        self.state.finish();
        self.terminated = true;
        self.take_events()
    }

    /// Converts a body-stream failure into one terminal failed assistant while
    /// preserving all successfully parsed content and usage.
    pub fn fail_transport(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<AssistantEvent> {
        self.fail_transport_error(TransportError::new(code, message))
    }

    /// Converts an enriched body-stream failure into one terminal failed
    /// assistant while retaining provider metadata.
    pub fn fail_transport_error(&mut self, error: TransportError) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.state.fail_public(PublicError {
            code: error.code,
            message: error.message,
            retryable: false,
            provider_code: error.provider_code,
            status: error.status,
            request_id: error.request_id.or_else(|| self.state.response_id.clone()),
        });
        self.terminated = true;
        self.take_events()
    }

    /// Converts post-establishment cancellation into one terminal cancelled
    /// assistant with partial content and the last response identifier.
    pub fn cancel(&mut self, message: impl Into<String>) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        let reason = CancellationReason {
            message: message.into(),
            request_id: self.state.response_id.clone(),
        };
        self.state.cancel(reason);
        self.terminated = true;
        self.take_events()
    }

    /// Returns whether a terminal event has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

struct ToolBlock {
    block_id: ContentBlockId,
    stream_index: Option<i64>,
    call_id: String,
    name: String,
    metadata_call_id: Option<String>,
    metadata_name: Option<String>,
    custom_input: Option<GrammarToolInputBuffer>,
}

struct GrammarToolInputBuffer {
    property: String,
    input: String,
    started: bool,
    closed: bool,
}

struct DecodeState {
    context: MistralConversationsDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    blocks: Vec<ContentBlockId>,
    active_content_block: Option<(ContentBlockId, ContentBlockKind)>,
    tools: Vec<ToolBlock>,
    tools_by_id: HashMap<String, usize>,
    reasoning_field_recorded: bool,
    next_replay_ordinal: u32,
    response_id: Option<String>,
    response_model: Option<ModelId>,
    usage: Usage,
    finish_reason: Option<AssistantFinishReason>,
    raw_finish_reason: Option<String>,
    finish_error: Option<String>,
}

impl DecodeState {
    fn new(context: MistralConversationsDecodeContext) -> Self {
        let mut state = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            events: Vec::new(),
            blocks: Vec::new(),
            active_content_block: None,
            tools: Vec::new(),
            tools_by_id: HashMap::new(),
            reasoning_field_recorded: false,
            next_replay_ordinal: 0,
            response_id: None,
            response_model: None,
            usage: Usage::zero(UsageSource::Unknown),
            finish_reason: None,
            raw_finish_reason: None,
            finish_error: None,
            context,
        };
        let started = AssistantEvent::MessageStarted {
            message_id: state.context.message_id.clone(),
            provider: state.context.provider.clone(),
            api: ApiId::new("mistral-conversations"),
            model: state.context.requested_model.clone(),
        };
        state
            .emit(started)
            .expect("MessageStarted is valid on a fresh assembler");
        state
    }

    fn decode_sse_event(&mut self, event: &[u8]) -> Result<(), MistralConversationsDecodeError> {
        let source = std::str::from_utf8(event).map_err(|error| {
            MistralConversationsDecodeError::new(format!("SSE body is not UTF-8: {error}"))
        })?;
        let Some(data) = sse_data_value(source) else {
            return Ok(());
        };
        if data == "[DONE]" {
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(&data).map_err(|error| {
            MistralConversationsDecodeError::new(format!("invalid SSE JSON data: {error}"))
        })?;
        let Some(chunk) = chunk.as_object() else {
            return Ok(());
        };
        self.decode_chunk(chunk)?;
        Ok(())
    }

    fn decode_chunk(
        &mut self,
        chunk: &Map<String, Value>,
    ) -> Result<(), MistralConversationsDecodeError> {
        let new_response_id = chunk
            .get("id")
            .and_then(Value::as_str)
            .filter(|response_id| !response_id.is_empty())
            .filter(|_| self.response_id.is_none())
            .map(str::to_owned);
        let new_response_model = chunk
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != self.context.requested_model.as_str())
            .filter(|_| self.response_model.is_none())
            .map(ModelId::new);
        if new_response_id.is_some() || new_response_model.is_some() {
            if let Some(response_id) = &new_response_id {
                self.response_id = Some(response_id.clone());
            }
            if let Some(response_model) = &new_response_model {
                self.response_model = Some(response_model.clone());
            }
            self.emit(AssistantEvent::ResponseMetadata {
                response_id: new_response_id,
                response_model: new_response_model,
                end_turn: None,
            })?;
        }

        if let Some(usage) = chunk.get("usage").filter(|value| !value.is_null()) {
            self.update_usage(usage)?;
        }

        let choice = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(Value::as_object);
        let Some(choice) = choice else {
            return Ok(());
        };
        if chunk.get("usage").is_none_or(Value::is_null)
            && let Some(usage) = choice.get("usage").filter(|value| !value.is_null())
        {
            self.update_usage(usage)?;
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.raw_finish_reason = Some(reason.to_owned());
            let (mapped, error) = map_finish_reason(reason);
            self.finish_reason = Some(mapped);
            self.finish_error = error;
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return Ok(());
        };

        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            let block_id = self.ensure_text_block()?;
            self.emit(AssistantEvent::TextDelta {
                block_id,
                delta: content.to_owned(),
            })?;
        }
        if let Some(content) = delta.get("content").and_then(Value::as_array) {
            for item in content.iter().filter_map(Value::as_object) {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            let block_id = self.ensure_text_block()?;
                            self.emit(AssistantEvent::TextDelta {
                                block_id,
                                delta: text.to_owned(),
                            })?;
                        }
                    }
                    Some("thinking") => {
                        let thinking = item
                            .get("thinking")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_object)
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<String>();
                        if !thinking.is_empty() {
                            let block_id = self.ensure_thinking_block()?;
                            self.emit(AssistantEvent::ThinkingDelta {
                                block_id,
                                delta: thinking,
                            })?;
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some((field, reasoning)) = first_reasoning_delta(delta) {
            let block_id = self.ensure_thinking_block()?;
            if !self.reasoning_field_recorded {
                let replay_field =
                    if self.context.provider.as_str() == "opencode-go" && field == "reasoning" {
                        "reasoning_content"
                    } else {
                        field
                    };
                self.record_replay(
                    ReplayTarget::ContentBlock(block_id.clone()),
                    OPENAI_CHAT_REASONING_FIELD_KIND,
                    ReplayDataOperation::ReplaceUtf8(replay_field.to_owned()),
                )?;
                self.reasoning_field_recorded = true;
            }
            self.emit(AssistantEvent::ThinkingDelta {
                block_id,
                delta: reasoning.to_owned(),
            })?;
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                if let Some(tool_call) = tool_call.as_object() {
                    self.decode_tool_delta(tool_call)?;
                }
            }
        }

        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details
                .iter()
                .filter(|detail| valid_reasoning_detail(detail))
            {
                let block_id = self.ensure_thinking_block()?;
                let bytes = OrderedJsonWriter::to_vec(&OrderedJsonValue::from(detail.clone()))
                    .map_err(|error| {
                        MistralConversationsDecodeError::new(format!(
                            "failed to preserve reasoning detail: {error}"
                        ))
                    })?;
                self.record_replay(
                    ReplayTarget::ContentBlock(block_id.clone()),
                    OPENAI_CHAT_REASONING_DETAIL_KIND,
                    ReplayDataOperation::ReplaceJsonBytes(bytes),
                )?;
            }
        }
        Ok(())
    }

    fn update_usage(&mut self, value: &Value) -> Result<(), MistralConversationsDecodeError> {
        let usage = value.as_object().ok_or_else(|| {
            MistralConversationsDecodeError::new("OpenAI usage value is not an object")
        })?;
        let prompt = usage_u64(usage, "prompt_tokens");
        let completion = usage_u64(usage, "completion_tokens");
        let prompt_details = usage
            .get("prompt_tokens_details")
            .and_then(Value::as_object);
        let completion_details = usage
            .get("completion_tokens_details")
            .and_then(Value::as_object);
        let cache_read = prompt_details
            .and_then(|details| optional_usage_u64(details, "cached_tokens"))
            .or_else(|| optional_usage_u64(usage, "prompt_cache_hit_tokens"))
            .or_else(|| optional_usage_u64(usage, "cached_tokens"))
            .unwrap_or(0);
        let cache_write = prompt_details
            .map(|details| usage_u64(details, "cache_write_tokens"))
            .unwrap_or(0);
        let reasoning = completion_details
            .map(|details| usage_u64(details, "reasoning_tokens"))
            .unwrap_or(0);
        self.usage = Usage {
            input_tokens: prompt
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output_tokens: completion,
            reasoning_tokens: Some(reasoning),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            cache_write_one_hour_tokens: None,
            total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
            source: UsageSource::ProviderReported,
        };
        self.emit(AssistantEvent::UsageUpdated {
            cumulative: self.usage.clone(),
        })
    }

    fn decode_tool_delta(
        &mut self,
        tool_call: &Map<String, Value>,
    ) -> Result<(), MistralConversationsDecodeError> {
        self.finish_active_content_block()?;
        let stream_index = tool_call.get("index").and_then(Value::as_i64);
        let incoming_id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");
        let function = tool_call.get("function").and_then(Value::as_object);
        let custom = tool_call.get("custom").and_then(Value::as_object);
        let incoming_name = function
            .and_then(|function| function.get("name"))
            .or_else(|| custom.and_then(|custom| custom.get("name")))
            .and_then(Value::as_str)
            .unwrap_or("");
        let existing_position = self
            .tools
            .iter()
            .position(|block| stream_index.is_some() && block.stream_index == stream_index)
            .or_else(|| {
                self.tools_by_id
                    .get(incoming_id)
                    .copied()
                    .filter(|_| !incoming_id.is_empty())
            });
        let position = if let Some(position) = existing_position {
            position
        } else {
            let block_id = self.start_block(ContentBlockKind::ToolCall)?;
            self.tools.push(ToolBlock {
                block_id,
                stream_index,
                call_id: String::new(),
                name: String::new(),
                metadata_call_id: None,
                metadata_name: None,
                custom_input: (custom.is_some() && function.is_none()).then(|| {
                    GrammarToolInputBuffer {
                        property: self
                            .context
                            .grammar_tool_input_properties
                            .get(incoming_name)
                            .cloned()
                            .unwrap_or_else(|| "input".into()),
                        input: String::new(),
                        started: false,
                        closed: false,
                    }
                }),
            });
            self.tools.len() - 1
        };

        if let Some(stream_index) = stream_index
            && self.tools[position].stream_index.is_none()
        {
            self.tools[position].stream_index = Some(stream_index);
        }
        if !incoming_id.is_empty() {
            if self.tools[position].call_id.is_empty() {
                self.tools[position].call_id = incoming_id.to_owned();
            }
            self.tools_by_id.insert(incoming_id.to_owned(), position);
        }
        if !incoming_name.is_empty() && self.tools[position].name.is_empty() {
            self.tools[position].name = incoming_name.to_owned();
        }
        if custom.is_some() && function.is_none() && self.tools[position].custom_input.is_none() {
            let property = self
                .context
                .grammar_tool_input_properties
                .get(self.tools[position].name.as_str())
                .cloned()
                .unwrap_or_else(|| "input".into());
            self.tools[position].custom_input = Some(GrammarToolInputBuffer {
                property,
                input: String::new(),
                started: false,
                closed: false,
            });
        }
        self.emit_tool_metadata_if_changed(position, false)?;

        if let Some(arguments) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
        {
            return self.emit(AssistantEvent::ToolArgumentsDelta {
                block_id: self.tools[position].block_id.clone(),
                delta: arguments.to_owned(),
            });
        }
        if let Some(arguments) = function
            .and_then(|function| function.get("arguments"))
            .filter(|value| value.is_object())
        {
            let delta = OrderedJsonWriter::stringify(&OrderedJsonValue::from(arguments.clone()))
                .map_err(|error| {
                    MistralConversationsDecodeError::new(format!(
                        "failed to encode tool arguments: {error}"
                    ))
                })?;
            return self.emit(AssistantEvent::ToolArgumentsDelta {
                block_id: self.tools[position].block_id.clone(),
                delta,
            });
        }
        let Some(input_delta) = custom
            .and_then(|custom| custom.get("input"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        let block_id = self.tools[position].block_id.clone();
        let delta = {
            let custom_input = self.tools[position]
                .custom_input
                .as_mut()
                .expect("custom input state is initialized above");
            let mut next_input = custom_input.input.clone();
            next_input.push_str(input_delta);
            append_grammar_tool_input_json_delta(custom_input, &next_input, false)?
        };
        if let Some(delta) = delta {
            self.emit(AssistantEvent::ToolArgumentsDelta { block_id, delta })?;
        }
        Ok(())
    }

    fn emit_tool_metadata_if_changed(
        &mut self,
        position: usize,
        force: bool,
    ) -> Result<(), MistralConversationsDecodeError> {
        if self.tools[position].call_id.is_empty() && !force {
            return Ok(());
        }
        let call_id_changed = self.tools[position].metadata_call_id.as_deref()
            != Some(self.tools[position].call_id.as_str());
        let name =
            (!self.tools[position].name.is_empty()).then(|| self.tools[position].name.clone());
        let name_changed = name != self.tools[position].metadata_name;
        if call_id_changed || name_changed {
            let block_id = self.tools[position].block_id.clone();
            let call_id = self.tools[position].call_id.clone();
            self.tools[position].metadata_call_id = Some(call_id.clone());
            self.tools[position].metadata_name = name.clone();
            self.emit(AssistantEvent::ToolCallMetadata {
                block_id,
                call_id: ToolCallId::new(call_id),
                name,
            })?;
        }
        Ok(())
    }

    fn ensure_text_block(&mut self) -> Result<ContentBlockId, MistralConversationsDecodeError> {
        if let Some((block_id, ContentBlockKind::Text)) = &self.active_content_block {
            return Ok(block_id.clone());
        }
        self.finish_active_content_block()?;
        let block_id = self.start_block(ContentBlockKind::Text)?;
        self.active_content_block = Some((block_id.clone(), ContentBlockKind::Text));
        Ok(block_id)
    }

    fn ensure_thinking_block(&mut self) -> Result<ContentBlockId, MistralConversationsDecodeError> {
        if let Some((block_id, ContentBlockKind::Thinking)) = &self.active_content_block {
            return Ok(block_id.clone());
        }
        self.finish_active_content_block()?;
        let block_id = self.start_block(ContentBlockKind::Thinking)?;
        self.active_content_block = Some((block_id.clone(), ContentBlockKind::Thinking));
        Ok(block_id)
    }

    fn finish_active_content_block(&mut self) -> Result<(), MistralConversationsDecodeError> {
        let Some((block_id, _)) = self.active_content_block.take() else {
            return Ok(());
        };
        self.emit(AssistantEvent::ContentBlockFinished { block_id })
    }

    fn start_block(
        &mut self,
        kind: ContentBlockKind,
    ) -> Result<ContentBlockId, MistralConversationsDecodeError> {
        let block_id = self.next_block_id();
        let content_index = u32::try_from(self.blocks.len())
            .map_err(|_| MistralConversationsDecodeError::new("too many content blocks"))?;
        self.blocks.push(block_id.clone());
        self.emit(AssistantEvent::ContentBlockStarted {
            block_id: block_id.clone(),
            content_index,
            kind,
        })?;
        Ok(block_id)
    }

    fn next_block_id(&self) -> ContentBlockId {
        ContentBlockId::new(format!(
            "openai-chat-block-{}-{}",
            self.context.message_id,
            self.blocks.len()
        ))
    }

    fn record_replay(
        &mut self,
        target: ReplayTarget,
        kind: &str,
        operation: ReplayDataOperation,
    ) -> Result<(), MistralConversationsDecodeError> {
        let ordinal = self.next_replay_ordinal;
        self.next_replay_ordinal = self.next_replay_ordinal.saturating_add(1);
        let item_id = ReplayItemId::new(format!(
            "openai-chat-replay-{}-{ordinal}",
            self.context.message_id
        ));
        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: item_id.clone(),
            ordinal,
            target,
            kind: ReplayKind::new(kind),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })?;
        self.emit(AssistantEvent::ReplayData {
            item_id: item_id.clone(),
            operation,
        })?;
        self.emit(AssistantEvent::ReplayItemFinished { item_id })
    }

    fn finish(&mut self) {
        if self.finish_reason.is_none() && !self.context.supports_finish_reason {
            self.finish_reason = Some(if self.tools.is_empty() {
                AssistantFinishReason::Stop
            } else {
                AssistantFinishReason::ToolUse
            });
        }
        if let Err(error) = self.finish_active_content_block() {
            self.fail(error.to_string());
            return;
        }
        for position in 0..self.tools.len() {
            if let Err(error) = self.emit_tool_metadata_if_changed(position, true) {
                self.fail(error.to_string());
                return;
            }
            if let Err(error) = self.finish_custom_tool_input(position) {
                self.fail(error.to_string());
                return;
            }
            let block_id = self.tools[position].block_id.clone();
            if let Err(error) = self.emit(AssistantEvent::ContentBlockFinished { block_id }) {
                self.fail(error.to_string());
                return;
            }
        }
        if self.finish_reason.is_none() {
            self.fail("Stream ended without finish_reason".into());
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
        let failed_assembler = assembler.clone();
        match assembler.finish_completed(finish) {
            Ok(message) => self.events.push(AssistantEvent::Finished { message }),
            Err(error) => {
                let message = failed_assembler.finish_failed(
                    PublicError {
                        code: "provider_protocol".into(),
                        message: format!("invalid completed OpenAI stream: {error}"),
                        retryable: false,
                        provider_code: None,
                        status: None,
                        request_id: self.response_id.clone(),
                    },
                    self.raw_finish_reason.clone(),
                );
                self.events.push(AssistantEvent::Failed { message });
            }
        }
    }

    fn fail(&mut self, message: String) {
        self.fail_public(PublicError {
            code: "provider_protocol".into(),
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
        let failed = assembler.finish_failed(error, self.raw_finish_reason.clone());
        self.events.push(AssistantEvent::Failed { message: failed });
    }

    fn finish_custom_tool_input(
        &mut self,
        position: usize,
    ) -> Result<(), MistralConversationsDecodeError> {
        let block_id = self.tools[position].block_id.clone();
        let delta = self.tools[position]
            .custom_input
            .as_mut()
            .map(|custom_input| {
                let input = custom_input.input.clone();
                append_grammar_tool_input_json_delta(custom_input, &input, true)
            })
            .transpose()?
            .flatten();
        if let Some(delta) = delta {
            self.emit(AssistantEvent::ToolArgumentsDelta { block_id, delta })?;
        }
        Ok(())
    }

    fn cancel(&mut self, reason: CancellationReason) {
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let cancelled = assembler.finish_cancelled(reason);
        self.events
            .push(AssistantEvent::Cancelled { message: cancelled });
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), MistralConversationsDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| MistralConversationsDecodeError::new("decoder already terminated"))?
            .apply(&event)
            .map_err(|error| {
                MistralConversationsDecodeError::new(format!("invalid decoded event: {error}"))
            })?;
        self.events.push(event);
        Ok(())
    }
}

fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputBuffer,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, MistralConversationsDecodeError> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(MistralConversationsDecodeError::new(format!(
            "grammar tool input for property {:?} changed after it was closed",
            buffer.property
        )));
    }
    let Some(input_delta) = next_input.strip_prefix(&buffer.input) else {
        return Err(MistralConversationsDecodeError::new(format!(
            "grammar tool input for property {:?} changed non-monotonically",
            buffer.property
        )));
    };
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        let property = serde_json::to_string(&buffer.property).map_err(|error| {
            MistralConversationsDecodeError::new(format!(
                "failed to encode grammar tool input property: {error}"
            ))
        })?;
        delta.push('{');
        delta.push_str(&property);
        delta.push_str(":\"");
        buffer.started = true;
    }
    let encoded_delta = serde_json::to_string(input_delta).map_err(|error| {
        MistralConversationsDecodeError::new(format!(
            "failed to encode grammar tool input delta: {error}"
        ))
    })?;
    delta.push_str(&encoded_delta[1..encoded_delta.len() - 1]);
    buffer.input.clear();
    buffer.input.push_str(next_input);
    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer.get(index..index + 4) == Some(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if buffer.get(index..index + 2) == Some(b"\n\n")
            || buffer.get(index..index + 2) == Some(b"\r\r")
        {
            return Some((index, 2));
        }
        index += 1;
    }
    None
}

fn sse_data_value(source: &str) -> Option<String> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let joined = normalized
        .split('\n')
        .filter_map(|line| line.strip_prefix("data:").map(trim_start_ecmascript))
        .collect::<Vec<_>>()
        .join("\n");
    let data = trim_ecmascript(&joined);
    (!data.is_empty()).then(|| data.to_owned())
}

fn first_reasoning_delta(delta: &Map<String, Value>) -> Option<(&'static str, &str)> {
    ["reasoning_content", "reasoning", "reasoning_text"]
        .into_iter()
        .find_map(|field| {
            delta
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| (field, value))
        })
}

fn valid_reasoning_detail(value: &Value) -> bool {
    let Some(value) = value.as_object() else {
        return false;
    };
    let common = value
        .get("id")
        .is_none_or(|value| value.is_null() || value.is_string())
        && value.get("format").is_none_or(Value::is_string)
        && value.get("index").is_none_or(Value::is_number);
    if !common {
        return false;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => value.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => value.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            value.get("text").is_some_and(Value::is_string)
                && value
                    .get("signature")
                    .is_none_or(|value| value.is_null() || value.is_string())
        }
        _ => false,
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

fn map_finish_reason(reason: &str) -> (AssistantFinishReason, Option<String>) {
    match reason {
        "stop" | "end" => (AssistantFinishReason::Stop, None),
        "length" | "model_length" => (AssistantFinishReason::Length, None),
        "function_call" | "tool_calls" => (AssistantFinishReason::ToolUse, None),
        other => (
            AssistantFinishReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}
