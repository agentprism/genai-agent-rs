//! OpenAI Responses SSE decoding into ordered, replay-aware assistant events.

use agentprism_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    AssistantMessageDiagnostic, CacheWriteRetention, CancellationReason, ContentBlockId,
    ContentBlockKind, Cost, Currency, MessageId, ModelId, ModelPricing,
    OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND, OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND,
    OPENAI_RESPONSES_REASONING_ITEM_KIND, OpenAiMessagePhase, OpenAiToolItemType, OrderedJsonValue,
    OrderedJsonWriter, ProviderId, PublicError, ReplayApplicability, ReplayDataOperation,
    ReplayItemId, ReplayKind, ReplayTarget, Timestamp, ToolCallId, Usage, UsageSource,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// Stable inputs for one OpenAI Responses decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiResponsesDecodeContext {
    /// Stable canonical message identifier.
    pub message_id: MessageId,
    /// Provider serving the request.
    pub provider: ProviderId,
    /// API family (`openai-responses` or `openai-codex-responses`).
    pub api: ApiId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Timestamp retained on the terminal assistant message.
    pub timestamp: Timestamp,
    /// Grammar-tool name to canonical string-argument property.
    pub grammar_tool_input_properties: BTreeMap<String, String>,
    /// Catalog pricing used by pi-ai for this response.
    pub pricing: ModelPricing,
    /// Original caller-selected service tier, before payload middleware.
    pub requested_service_tier: Option<String>,
}

/// Malformed or inconsistent Responses event data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiResponsesDecodeError(String);

impl OpenAiResponsesDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for OpenAiResponsesDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OpenAiResponsesDecodeError {}

/// Decodes one complete OpenAI Responses SSE transcript.
pub fn decode_openai_responses_sse(
    body: &[u8],
    context: OpenAiResponsesDecodeContext,
) -> Vec<AssistantEvent> {
    let mut decoder = OpenAiResponsesSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.push(body));
    events.extend(decoder.finish());
    events
}

/// Incremental Responses SSE decoder.
pub struct OpenAiResponsesSseDecoder {
    state: DecodeState,
    buffer: Vec<u8>,
    terminated: bool,
}

impl OpenAiResponsesSseDecoder {
    /// Creates a decoder and queues `MessageStarted`.
    pub fn new(context: OpenAiResponsesDecodeContext) -> Self {
        Self {
            state: DecodeState::new(context),
            buffer: Vec::new(),
            terminated: false,
        }
    }

    /// Drains currently queued events.
    pub fn take_events(&mut self) -> Vec<AssistantEvent> {
        std::mem::take(&mut self.state.events)
    }

    /// Seeds a redacted transport recovery diagnostic before provider body
    /// events are consumed.
    pub fn add_diagnostic(
        &mut self,
        diagnostic: AssistantMessageDiagnostic,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        if self.terminated {
            return Err(OpenAiResponsesDecodeError::new(
                "cannot add a diagnostic after stream termination",
            ));
        }
        self.state
            .emit(AssistantEvent::DiagnosticAdded { diagnostic })
    }

    /// Consumes an arbitrary body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let codex = self.state.is_codex();
        while let Some((boundary, separator)) = find_sse_boundary(&self.buffer, codex) {
            let event = self.buffer[..boundary].to_vec();
            self.buffer.drain(..boundary + separator);
            if let Err(error) = self.state.decode_sse_event(&event) {
                self.state.fail(error.to_string());
                self.terminated = true;
                break;
            }
            if self.state.assembler.is_none() {
                self.terminated = true;
                break;
            }
        }
        self.take_events()
    }

    /// Handles body EOF and requires a terminal Responses event.
    pub fn finish(&mut self) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        let codex = self.state.context.api.as_str() == "openai-codex-responses";
        if !codex && !self.buffer.iter().all(u8::is_ascii_whitespace) {
            let event = std::mem::take(&mut self.buffer);
            if let Err(error) = self.state.decode_sse_event(&event) {
                self.state.fail(error.to_string());
                self.terminated = true;
                return self.take_events();
            }
        } else {
            self.buffer.clear();
        }
        if self.state.assembler.is_some() {
            self.state
                .fail("OpenAI Responses stream ended before a terminal response event".into());
        }
        self.terminated = true;
        self.take_events()
    }

    /// Commits a body-transport failure.
    pub fn fail_transport(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<AssistantEvent> {
        if !self.terminated {
            self.state.fail_public(
                PublicError {
                    code: code.into(),
                    message: message.into(),
                    retryable: false,
                    provider_code: None,
                    status: None,
                    request_id: self.state.response_id.clone(),
                },
                None,
            );
            self.terminated = true;
        }
        self.take_events()
    }

    /// Commits post-establishment cancellation.
    pub fn cancel(&mut self, message: impl Into<String>) -> Vec<AssistantEvent> {
        if !self.terminated {
            let reason = CancellationReason {
                message: message.into(),
                request_id: self.state.response_id.clone(),
            };
            self.state.cancel(reason);
            self.terminated = true;
        }
        self.take_events()
    }

    /// Returns whether a terminal event has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotKind {
    Reasoning,
    Message,
    Function,
    Custom,
}

struct OutputSlot {
    output_index: u32,
    kind: SlotKind,
    block_id: ContentBlockId,
    replay_item_id: ReplayItemId,
    call_id: Option<ToolCallId>,
    item_call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    custom_buffer_input: String,
    custom_started: bool,
    custom_closed: bool,
    visible: String,
}

struct PendingReasoningReplay {
    output_index: u32,
    item_id: ReplayItemId,
    payload: Vec<u8>,
}

struct DecodeState {
    context: OpenAiResponsesDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    slots: HashMap<u64, OutputSlot>,
    pending_reasoning_replay: Vec<PendingReasoningReplay>,
    block_order: Vec<ContentBlockId>,
    active_indexless_slot: Option<u64>,
    next_indexless_slot: u64,
    response_id: Option<String>,
    response_model: Option<ModelId>,
    end_turn: Option<bool>,
    saw_tool_call: bool,
    usage: Usage,
    cost: Option<Cost>,
}

impl DecodeState {
    fn new(context: OpenAiResponsesDecodeContext) -> Self {
        let mut state = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            events: Vec::new(),
            slots: HashMap::new(),
            pending_reasoning_replay: Vec::new(),
            block_order: Vec::new(),
            active_indexless_slot: None,
            next_indexless_slot: u64::from(u32::MAX) + 1,
            response_id: None,
            response_model: None,
            end_turn: None,
            saw_tool_call: false,
            usage: Usage::zero(UsageSource::Unknown),
            cost: None,
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

    fn decode_sse_event(&mut self, event: &[u8]) -> Result<(), OpenAiResponsesDecodeError> {
        let source = std::str::from_utf8(event).map_err(|error| {
            OpenAiResponsesDecodeError::new(format!("SSE body is not UTF-8: {error}"))
        })?;
        let Some(data) = sse_data_value(source) else {
            return Ok(());
        };
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).map_err(|error| {
            OpenAiResponsesDecodeError::new(format!("invalid SSE JSON data: {error}"))
        })?;
        let Some(event) = value.as_object() else {
            return Ok(());
        };
        self.decode_event(event)
    }

    fn decode_event(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.created" => {
                if let Some(response) = object_field(event, "response") {
                    self.capture_response_id(response)?;
                }
            }
            "response.output_item.added" => {
                let item = object_required(event, "item")?;
                let Some((slot, output_index)) = self.output_slot(event, true)? else {
                    return Ok(());
                };
                self.ensure_slot(slot, output_index, item)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_text(slot, string_field(event, "delta"), true)?;
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_text(slot, "\n\n", true)?;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_text(slot, string_field(event, "delta"), false)?;
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_arguments(slot, string_field(event, "delta"))?;
                }
            }
            "response.function_call_arguments.done" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.finish_arguments(slot, string_field(event, "arguments"))?;
                }
            }
            "response.custom_tool_call_input.delta" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_custom_input(slot, string_field(event, "delta"), false)?;
                }
            }
            "response.custom_tool_call_input.done" => {
                if let Some((slot, _)) = self.output_slot(event, false)? {
                    self.append_custom_input(slot, string_field(event, "input"), true)?;
                }
            }
            "response.output_item.done" => {
                let item = object_required(event, "item")?;
                let indexless = event.get("output_index").is_none();
                let Some((slot, output_index)) = self.output_slot(event, true)? else {
                    return Ok(());
                };
                self.finish_item(slot, output_index, item)?;
                if indexless {
                    self.active_indexless_slot = None;
                }
            }
            "response.done" if self.is_codex() => {
                let response = object_required(event, "response")?;
                self.finish_response(response)?;
            }
            "response.done" => {}
            "response.completed" | "response.incomplete" => {
                let response = object_required(event, "response")?;
                self.finish_response(response)?;
            }
            "response.failed" => {
                if self.is_codex() {
                    let response = object_field(event, "response");
                    let error = response
                        .and_then(|response| response.get("error"))
                        .and_then(Value::as_object);
                    let code = error
                        .and_then(|error| error.get("code"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let message = error
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .unwrap_or("Codex response failed")
                        .to_owned();
                    self.fail_codex(message, code);
                    return Ok(());
                }
                let response = object_field(event, "response");
                let error = response
                    .and_then(|response| response.get("error"))
                    .and_then(Value::as_object);
                let message = error.map_or_else(
                    || {
                        response
                            .and_then(|response| response.get("incomplete_details"))
                            .and_then(Value::as_object)
                            .and_then(|details| details.get("reason"))
                            .and_then(Value::as_str)
                            .map_or_else(
                                || "Unknown error (no error details in response)".to_owned(),
                                |reason| format!("incomplete: {reason}"),
                            )
                    },
                    |error| {
                        format!(
                            "{}: {}",
                            error
                                .get("code")
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                                .unwrap_or("unknown"),
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                                .unwrap_or("no message")
                        )
                    },
                );
                self.fail_with_raw(
                    message,
                    response
                        .and_then(|response| response.get("status"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
            }
            "error" => {
                if self.is_codex() {
                    let nested = event.get("error").and_then(Value::as_object);
                    let code = event
                        .get("code")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            nested
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_str)
                        })
                        .map(str::to_owned);
                    let message = event
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            nested
                                .and_then(|error| error.get("message"))
                                .and_then(Value::as_str)
                        })
                        .filter(|message| !message.is_empty())
                        .map(str::to_owned)
                        .or_else(|| code.clone())
                        .unwrap_or_else(|| Value::Object(event.clone()).to_string());
                    self.fail_codex(format!("Codex error: {message}"), code);
                } else {
                    let code = nullable_display(event.get("code"));
                    let message = nullable_display(event.get("message"));
                    self.fail(format!("Error Code {code}: {message}"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn output_slot(
        &mut self,
        event: &Map<String, Value>,
        create_indexless: bool,
    ) -> Result<Option<(u64, u32)>, OpenAiResponsesDecodeError> {
        if let Some(value) = event.get("output_index") {
            let index = value
                .as_u64()
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| {
                    OpenAiResponsesDecodeError::new(
                        "Responses event contains an invalid output_index",
                    )
                })?;
            return Ok(Some((u64::from(index), index)));
        }

        if self.context.api.as_str() != "openai-codex-responses" {
            return Err(OpenAiResponsesDecodeError::new(
                "Responses event omits a valid output_index",
            ));
        }
        if let Some(slot) = self.active_indexless_slot {
            let output_index = self
                .slots
                .get(&slot)
                .map(|slot| slot.output_index)
                .unwrap_or_else(|| u32::try_from(self.block_order.len()).unwrap_or(u32::MAX));
            return Ok(Some((slot, output_index)));
        }
        if !create_indexless {
            // Pinned Pi indexes these events with JavaScript's `undefined` Map
            // key. A delta without a corresponding index-less added item is
            // therefore ignored rather than treated as a protocol failure.
            return Ok(None);
        }

        let slot = self.next_indexless_slot;
        self.next_indexless_slot = self.next_indexless_slot.checked_add(1).ok_or_else(|| {
            OpenAiResponsesDecodeError::new("too many index-less Codex output items")
        })?;
        let output_index = u32::try_from(self.block_order.len())
            .map_err(|_| OpenAiResponsesDecodeError::new("too many content blocks"))?;
        self.active_indexless_slot = Some(slot);
        Ok(Some((slot, output_index)))
    }

    fn capture_response_id(
        &mut self,
        response: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        if self.set_response_id(response) {
            self.emit_response_metadata()?;
        }
        Ok(())
    }

    fn set_response_id(&mut self, response: &Map<String, Value>) -> bool {
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let changed = id.is_some_and(|id| self.response_id.as_deref() != Some(id));
        if let Some(id) = id {
            self.response_id = Some(id.to_owned());
        }
        changed
    }

    fn update_terminal_metadata(
        &mut self,
        response: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let response_id_changed = self.set_response_id(response);
        let end_turn = self
            .is_codex()
            .then(|| response.get("end_turn").and_then(Value::as_bool))
            .flatten();
        let end_turn_changed = end_turn.is_some_and(|end_turn| self.end_turn != Some(end_turn));
        if let Some(end_turn) = end_turn {
            self.end_turn = Some(end_turn);
        }
        if response_id_changed || end_turn_changed {
            self.emit_response_metadata()?;
        }
        Ok(())
    }

    fn emit_response_metadata(&mut self) -> Result<(), OpenAiResponsesDecodeError> {
        self.emit(AssistantEvent::ResponseMetadata {
            response_id: self.response_id.clone(),
            response_model: self.response_model.clone(),
            end_turn: self.end_turn,
        })
    }

    fn ensure_slot(
        &mut self,
        index: u64,
        output_index: u32,
        item: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        if self.slots.contains_key(&index) {
            return Ok(());
        }
        let item_kind = string_field(item, "type");
        let kind = match item_kind {
            "reasoning" => SlotKind::Reasoning,
            "message" => SlotKind::Message,
            "function_call" => SlotKind::Function,
            "custom_tool_call" => SlotKind::Custom,
            _ => return Ok(()),
        };
        let block_id = ContentBlockId::new(format!(
            "openai-responses-block-{}-{output_index}",
            self.context.message_id
        ));
        let replay_item_id = ReplayItemId::new(format!(
            "openai-responses-replay-{}-{output_index}",
            self.context.message_id
        ));
        let content_index = u32::try_from(self.block_order.len())
            .map_err(|_| OpenAiResponsesDecodeError::new("too many content blocks"))?;
        self.block_order.push(block_id.clone());

        let item_call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item_id = item.get("id").and_then(Value::as_str);
        let call_id = item_call_id.as_deref().map(|call_id| {
            item_id.map_or_else(
                || ToolCallId::new(call_id),
                |item_id| ToolCallId::new(format!("{call_id}|{item_id}")),
            )
        });
        let name = item.get("name").and_then(Value::as_str).map(str::to_owned);
        let arguments = match kind {
            SlotKind::Function => item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            SlotKind::Custom => "",
            SlotKind::Reasoning | SlotKind::Message => "",
        }
        .to_owned();

        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: replay_item_id.clone(),
            ordinal: output_index,
            target: ReplayTarget::ProviderOutputItem { output_index },
            kind: ReplayKind::new(match kind {
                SlotKind::Reasoning => OPENAI_RESPONSES_REASONING_ITEM_KIND,
                SlotKind::Message => OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND,
                SlotKind::Function | SlotKind::Custom => {
                    OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND
                }
            }),
            applicability: if matches!(kind, SlotKind::Function | SlotKind::Custom) {
                ReplayApplicability::ExactProviderApi
            } else {
                ReplayApplicability::ExactProviderApiModel
            },
        })?;
        self.emit(AssistantEvent::ContentBlockStarted {
            block_id: block_id.clone(),
            content_index,
            kind: match kind {
                SlotKind::Reasoning => ContentBlockKind::Thinking,
                SlotKind::Message => ContentBlockKind::Text,
                SlotKind::Function | SlotKind::Custom => ContentBlockKind::ToolCall,
            },
        })?;
        if matches!(kind, SlotKind::Function | SlotKind::Custom) {
            self.emit(AssistantEvent::ToolCallMetadata {
                block_id: block_id.clone(),
                call_id: call_id.clone().unwrap_or_default(),
                name: name.clone(),
            })?;
        }
        let initial_function_arguments =
            (kind == SlotKind::Function && !arguments.is_empty()).then(|| arguments.clone());
        let initial_custom_input = (kind == SlotKind::Custom)
            .then(|| {
                item.get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|input| !input.is_empty());
        self.saw_tool_call |= matches!(kind, SlotKind::Function | SlotKind::Custom);
        self.slots.insert(
            index,
            OutputSlot {
                output_index,
                kind,
                block_id,
                replay_item_id,
                call_id,
                item_call_id,
                name,
                arguments,
                custom_buffer_input: String::new(),
                custom_started: false,
                custom_closed: false,
                visible: String::new(),
            },
        );
        if let Some(delta) = initial_function_arguments {
            self.emit(AssistantEvent::ToolArgumentsDelta {
                block_id: self
                    .slots
                    .get(&index)
                    .expect("slot inserted")
                    .block_id
                    .clone(),
                delta,
            })?;
        }
        if let Some(input) = initial_custom_input {
            self.append_custom_input(index, &input, false)?;
        }
        Ok(())
    }

    fn append_text(
        &mut self,
        index: u64,
        delta: &str,
        thinking: bool,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(());
        };
        if thinking != (slot.kind == SlotKind::Reasoning)
            || (!thinking && slot.kind != SlotKind::Message)
        {
            return Ok(());
        }
        slot.visible.push_str(delta);
        let block_id = slot.block_id.clone();
        self.emit(if thinking {
            AssistantEvent::ThinkingDelta {
                block_id,
                delta: delta.to_owned(),
            }
        } else {
            AssistantEvent::TextDelta {
                block_id,
                delta: delta.to_owned(),
            }
        })
    }

    fn append_arguments(
        &mut self,
        index: u64,
        delta: &str,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(());
        };
        if slot.kind != SlotKind::Function {
            return Ok(());
        }
        slot.arguments.push_str(delta);
        let block_id = slot.block_id.clone();
        self.emit(AssistantEvent::ToolArgumentsDelta {
            block_id,
            delta: delta.to_owned(),
        })
    }

    fn finish_arguments(
        &mut self,
        index: u64,
        complete: &str,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(());
        };
        if slot.kind != SlotKind::Function {
            return Ok(());
        }
        let previous = slot.arguments.clone();
        let delta = complete
            .strip_prefix(&previous)
            .filter(|delta| !delta.is_empty())
            .map(str::to_owned);
        let replacement =
            (complete != previous && !complete.starts_with(&previous)).then(|| complete.to_owned());
        slot.arguments = complete.to_owned();
        let block_id = slot.block_id.clone();
        if let Some(delta) = delta {
            self.emit(AssistantEvent::ToolArgumentsDelta { block_id, delta })?;
        } else if let Some(arguments) = replacement {
            self.emit(AssistantEvent::ToolArgumentsReplaced {
                block_id,
                arguments,
            })?;
        }
        Ok(())
    }

    fn append_custom_input(
        &mut self,
        index: u64,
        input: &str,
        complete: bool,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(());
        };
        if slot.kind != SlotKind::Custom {
            return Ok(());
        }
        if slot.custom_closed {
            if complete && input == slot.custom_buffer_input {
                return Ok(());
            }
            return Err(OpenAiResponsesDecodeError::new(format!(
                "grammar tool input for property \"{}\" changed after it was closed",
                slot.name
                    .as_ref()
                    .and_then(|name| self.context.grammar_tool_input_properties.get(name))
                    .map(String::as_str)
                    .unwrap_or("input")
            )));
        }
        let next = if complete {
            input.to_owned()
        } else {
            format!("{}{}", slot.arguments, input)
        };
        let property = slot
            .name
            .as_ref()
            .and_then(|name| self.context.grammar_tool_input_properties.get(name))
            .map(String::as_str)
            .unwrap_or("input");
        let Some(input_delta) = next.strip_prefix(&slot.custom_buffer_input) else {
            return Err(OpenAiResponsesDecodeError::new(format!(
                "grammar tool input for property \"{property}\" changed non-monotonically"
            )));
        };
        if !complete && input_delta.is_empty() {
            return Ok(());
        }
        let mut delta = String::new();
        if !slot.custom_started {
            let property = serde_json::to_string(property).map_err(|error| {
                OpenAiResponsesDecodeError::new(format!(
                    "failed to encode grammar property: {error}"
                ))
            })?;
            delta.push('{');
            delta.push_str(&property);
            delta.push_str(":\"");
            slot.custom_started = true;
        }
        let encoded = serde_json::to_string(input_delta).map_err(|error| {
            OpenAiResponsesDecodeError::new(format!("failed to encode grammar input: {error}"))
        })?;
        delta.push_str(&encoded[1..encoded.len() - 1]);
        slot.custom_buffer_input = next.clone();
        slot.arguments = next;
        if complete {
            delta.push_str("\"}");
            slot.custom_closed = true;
        }
        let block_id = slot.block_id.clone();
        if !delta.is_empty() {
            self.emit(AssistantEvent::ToolArgumentsDelta { block_id, delta })?;
        }
        Ok(())
    }

    fn finish_item(
        &mut self,
        index: u64,
        output_index: u32,
        item: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        self.ensure_slot(index, output_index, item)?;
        let Some(slot_kind) = self.slots.get(&index).map(|slot| slot.kind) else {
            return Ok(());
        };
        match slot_kind {
            SlotKind::Reasoning => {
                let final_text = reasoning_text(item);
                if !final_text.is_empty() {
                    self.complete_visible(index, &final_text, true)?;
                }
            }
            SlotKind::Message => {
                let final_text = message_text(item);
                self.complete_visible(index, &final_text, false)?;
            }
            SlotKind::Function => {
                let item_arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|arguments| !arguments.is_empty());
                let prior = self
                    .slots
                    .get(&index)
                    .map(|slot| slot.arguments.clone())
                    .filter(|arguments| !arguments.is_empty());
                let arguments = item_arguments
                    .map(str::to_owned)
                    .or(prior)
                    .unwrap_or_else(|| "{}".to_owned());
                self.finish_arguments(index, &arguments)?;
            }
            SlotKind::Custom => {
                let input = item.get("input").and_then(Value::as_str).map(str::to_owned);
                let input = input.unwrap_or_else(|| {
                    self.slots
                        .get(&index)
                        .map_or_else(String::new, |slot| slot.arguments.clone())
                });
                self.append_custom_input(index, &input, true)?;
            }
        }

        let slot = self.slots.get(&index).expect("slot ensured");
        let payload = match slot.kind {
            SlotKind::Reasoning => {
                OrderedJsonWriter::to_vec(&OrderedJsonValue::from(Value::Object(item.clone())))
                    .map_err(|error| {
                        OpenAiResponsesDecodeError::new(format!(
                            "failed to serialize reasoning item: {error}"
                        ))
                    })?
            }
            SlotKind::Message => {
                let mut value = agentprism_ai::OrderedJsonObject::new();
                value.insert("id", string_field(item, "id"));
                if let Some(phase) = item.get("phase").and_then(Value::as_str) {
                    value.insert("phase", phase);
                }
                value.insert("block_id", slot.block_id.as_str());
                OrderedJsonWriter::to_vec(&value.into()).map_err(|error| {
                    OpenAiResponsesDecodeError::new(format!(
                        "failed to serialize message identity: {error}"
                    ))
                })?
            }
            SlotKind::Function | SlotKind::Custom => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or(slot.item_call_id.as_deref())
                    .unwrap_or_default();
                let canonical = slot
                    .call_id
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| ToolCallId::new(call_id));
                let mut value = agentprism_ai::OrderedJsonObject::new();
                value.insert("tool_call_id", canonical.as_str());
                value.insert("call_id", call_id);
                match item.get("id").and_then(Value::as_str) {
                    Some(id) => value.insert("item_id", id),
                    None => value.insert("item_id", OrderedJsonValue::Null),
                };
                match item.get("namespace").and_then(Value::as_str) {
                    Some(namespace) => value.insert("namespace", namespace),
                    None => value.insert("namespace", OrderedJsonValue::Null),
                };
                value.insert(
                    "type",
                    if slot.kind == SlotKind::Custom {
                        "custom_tool_call"
                    } else {
                        "function_call"
                    },
                );
                OrderedJsonWriter::to_vec(&value.into()).map_err(|error| {
                    OpenAiResponsesDecodeError::new(format!(
                        "failed to serialize function identity: {error}"
                    ))
                })?
            }
        };
        let item_id = slot.replay_item_id.clone();
        self.emit(AssistantEvent::ReplayData {
            item_id: item_id.clone(),
            operation: ReplayDataOperation::ReplaceJsonBytes(payload.clone()),
        })?;
        if slot_kind == SlotKind::Reasoning {
            self.pending_reasoning_replay.push(PendingReasoningReplay {
                output_index,
                item_id,
                payload,
            });
        } else {
            self.emit(AssistantEvent::ReplayItemFinished { item_id })?;
        }
        let block_id = self
            .slots
            .get(&index)
            .expect("slot exists")
            .block_id
            .clone();
        self.emit(AssistantEvent::ContentBlockFinished { block_id })?;
        self.slots.remove(&index);
        Ok(())
    }

    fn complete_visible(
        &mut self,
        index: u64,
        complete: &str,
        thinking: bool,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(());
        };
        let delta = complete
            .strip_prefix(&slot.visible)
            .filter(|delta| !delta.is_empty())
            .map(str::to_owned);
        let replacement = (complete != slot.visible && !complete.starts_with(&slot.visible))
            .then(|| complete.to_owned());
        slot.visible = complete.to_owned();
        let block_id = slot.block_id.clone();
        if let Some(delta) = delta {
            self.emit(if thinking {
                AssistantEvent::ThinkingDelta { block_id, delta }
            } else {
                AssistantEvent::TextDelta { block_id, delta }
            })?;
        } else if let Some(complete) = replacement {
            self.emit(if thinking {
                AssistantEvent::ThinkingReplaced {
                    block_id,
                    thinking: complete,
                }
            } else {
                AssistantEvent::TextReplaced {
                    block_id,
                    text: complete,
                }
            })?;
        }
        Ok(())
    }

    fn finish_response(
        &mut self,
        response: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        self.update_terminal_metadata(response)?;
        self.backfill_reasoning_replay(response)?;
        self.finish_pending_reasoning_replay()?;
        if let Some(usage) = response.get("usage").and_then(Value::as_object) {
            let input_total = unsigned_field(usage, "input_tokens");
            let input_details = usage.get("input_tokens_details").and_then(Value::as_object);
            let cache_read =
                input_details.map_or(0, |details| unsigned_field(details, "cached_tokens"));
            let cache_write =
                input_details.map_or(0, |details| unsigned_field(details, "cache_write_tokens"));
            let output = unsigned_field(usage, "output_tokens");
            let reasoning = usage
                .get("output_tokens_details")
                .and_then(Value::as_object)
                .map(|details| unsigned_field(details, "reasoning_tokens"));
            self.usage = Usage {
                input_tokens: input_total
                    .saturating_sub(cache_read)
                    .saturating_sub(cache_write),
                output_tokens: output,
                reasoning_tokens: reasoning,
                cache_read_tokens: Some(cache_read),
                cache_write_tokens: Some(cache_write),
                cache_write_one_hour_tokens: None,
                total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
                source: UsageSource::ProviderReported,
            };
            self.emit(AssistantEvent::UsageUpdated {
                cumulative: self.usage.clone(),
            })?;
        }
        let service_tier = self.resolve_service_tier(response);
        let (multiplier_numerator, multiplier_denominator) =
            service_tier_multiplier(&self.context.requested_model, service_tier.as_deref());
        self.cost = Some(
            self.context
                .pricing
                .calculate_cost_with_multiplier(
                    &self.usage,
                    Currency::usd(),
                    CacheWriteRetention::Default,
                    multiplier_numerator,
                    multiplier_denominator,
                )
                .map_err(|error| {
                    OpenAiResponsesDecodeError::new(format!(
                        "failed to calculate OpenAI Responses cost: {error}"
                    ))
                })?,
        );
        let mut indexes = self.slots.keys().copied().collect::<Vec<_>>();
        indexes.sort_by_key(|index| {
            self.slots
                .get(index)
                .map_or(u32::MAX, |slot| slot.output_index)
        });
        for index in indexes {
            let block_id = self
                .slots
                .get(&index)
                .expect("slot exists")
                .block_id
                .clone();
            self.emit(AssistantEvent::ContentBlockFinished { block_id })?;
        }
        let reported_status = response.get("status").and_then(Value::as_str);
        let status = if self.context.api.as_str() == "openai-codex-responses"
            && reported_status.is_some_and(|status| {
                !matches!(
                    status,
                    "completed" | "incomplete" | "failed" | "cancelled" | "queued" | "in_progress"
                )
            }) {
            None
        } else {
            reported_status
        };
        let incomplete_reason = response
            .get("incomplete_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str);
        let raw = match (status, incomplete_reason) {
            (Some(status), Some(reason)) => Some(format!("{status}.{reason}")),
            (Some(status), None) => Some(status.to_owned()),
            _ => None,
        };
        let mut reason = match status {
            None | Some("completed" | "in_progress" | "queued") => AssistantFinishReason::Stop,
            Some("incomplete") if incomplete_reason == Some("max_output_tokens") => {
                AssistantFinishReason::Length
            }
            Some("incomplete") => {
                self.fail_with_raw(
                    incomplete_reason.map_or_else(
                        || "Response incomplete without a provider reason".to_owned(),
                        |reason| format!("Response incomplete: {reason}"),
                    ),
                    raw,
                );
                return Ok(());
            }
            Some("failed" | "cancelled") => {
                self.fail_with_raw(
                    format!("OpenAI response status was {}", status.unwrap_or_default()),
                    raw,
                );
                return Ok(());
            }
            Some(status) => {
                self.fail_with_raw(format!("Unhandled stop reason: {status}"), raw);
                return Ok(());
            }
        };
        if reason == AssistantFinishReason::Stop && self.saw_tool_call {
            reason = AssistantFinishReason::ToolUse;
        }
        let finish = AssistantFinish {
            reason,
            raw_provider_reason: raw.clone(),
            error: None,
        };
        let Some(assembler) = self.assembler.take() else {
            return Ok(());
        };
        let failed = assembler.clone();
        match assembler.finish_completed(finish) {
            Ok(mut message) => {
                message.cost = self.cost.clone();
                self.events.push(AssistantEvent::Finished { message });
            }
            Err(error) => {
                let mut message = failed.finish_failed(
                    PublicError {
                        code: "provider_protocol".into(),
                        message: format!("invalid completed OpenAI Responses stream: {error}"),
                        retryable: false,
                        provider_code: None,
                        status: None,
                        request_id: self.response_id.clone(),
                    },
                    raw,
                );
                message.cost = self.cost.clone();
                self.events.push(AssistantEvent::Failed { message });
            }
        }
        Ok(())
    }

    fn resolve_service_tier(&self, response: &Map<String, Value>) -> Option<String> {
        let response_tier = response
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if self.is_codex()
            && response_tier.as_deref() == Some("default")
            && self
                .context
                .requested_service_tier
                .as_deref()
                .is_some_and(|tier| matches!(tier, "flex" | "priority"))
        {
            return self.context.requested_service_tier.clone();
        }
        response_tier.or_else(|| self.context.requested_service_tier.clone())
    }

    fn fail(&mut self, message: String) {
        self.fail_with_raw(message, None);
    }

    fn is_codex(&self) -> bool {
        self.context.api.as_str() == "openai-codex-responses"
    }

    fn fail_codex(&mut self, message: String, provider_code: Option<String>) {
        self.fail_public(
            PublicError {
                code: "provider_rejected".into(),
                message,
                retryable: false,
                provider_code,
                status: None,
                request_id: self.response_id.clone(),
            },
            None,
        );
    }

    fn fail_with_raw(&mut self, message: String, raw_provider_reason: Option<String>) {
        self.fail_public(
            PublicError {
                code: "provider_protocol".into(),
                message,
                retryable: false,
                provider_code: None,
                status: None,
                request_id: self.response_id.clone(),
            },
            raw_provider_reason,
        );
    }

    fn fail_public(&mut self, error: PublicError, raw_provider_reason: Option<String>) {
        let _ = self.finish_pending_reasoning_replay();
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let mut message = assembler.finish_failed(error, raw_provider_reason);
        message.cost = self.cost.clone();
        self.events.push(AssistantEvent::Failed { message });
    }

    fn cancel(&mut self, reason: CancellationReason) {
        let _ = self.finish_pending_reasoning_replay();
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        self.events.push(AssistantEvent::Cancelled {
            message: assembler.finish_cancelled(reason),
        });
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), OpenAiResponsesDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| OpenAiResponsesDecodeError::new("decoder already terminated"))?
            .apply(&event)
            .map_err(|error| {
                OpenAiResponsesDecodeError::new(format!("invalid decoded event: {error}"))
            })?;
        self.events.push(event);
        Ok(())
    }

    fn backfill_reasoning_replay(
        &mut self,
        response: &Map<String, Value>,
    ) -> Result<(), OpenAiResponsesDecodeError> {
        let terminal_items = response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter(|item| string_field(item, "type") == "reasoning")
            .filter_map(|item| {
                let encrypted = item.get("encrypted_content")?.as_str()?;
                if encrypted.is_empty() {
                    return None;
                }
                Some((item.get("id")?.as_str()?.to_owned(), encrypted.to_owned()))
            })
            .collect::<HashMap<_, _>>();
        if terminal_items.is_empty() {
            return Ok(());
        }
        let mut updates = Vec::new();
        for pending in &mut self.pending_reasoning_replay {
            let item_id = pending.item_id.clone();
            let payload = &mut pending.payload;
            let mut value: Value = serde_json::from_slice(payload).map_err(|error| {
                OpenAiResponsesDecodeError::new(format!(
                    "invalid pending reasoning replay JSON: {error}"
                ))
            })?;
            let Some(item) = value.as_object_mut() else {
                continue;
            };
            if item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
            {
                continue;
            }
            let Some(encrypted) = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| terminal_items.get(id))
            else {
                continue;
            };
            item.insert("encrypted_content".into(), Value::String(encrypted.clone()));
            *payload =
                OrderedJsonWriter::to_vec(&OrderedJsonValue::from(value)).map_err(|error| {
                    OpenAiResponsesDecodeError::new(format!(
                        "failed to serialize backfilled reasoning item: {error}"
                    ))
                })?;
            updates.push((pending.output_index, item_id, payload.clone()));
        }
        updates.sort_by_key(|(output_index, _, _)| *output_index);
        for (_, item_id, payload) in updates {
            self.emit(AssistantEvent::ReplayData {
                item_id,
                operation: ReplayDataOperation::ReplaceJsonBytes(payload),
            })?;
        }
        Ok(())
    }

    fn finish_pending_reasoning_replay(&mut self) -> Result<(), OpenAiResponsesDecodeError> {
        let mut pending = std::mem::take(&mut self.pending_reasoning_replay);
        pending.sort_by_key(|item| item.output_index);
        for item in pending {
            self.emit(AssistantEvent::ReplayItemFinished {
                item_id: item.item_id,
            })?;
        }
        Ok(())
    }
}

fn object_field<'a>(value: &'a Map<String, Value>, name: &str) -> Option<&'a Map<String, Value>> {
    value.get(name).and_then(Value::as_object)
}

fn object_required<'a>(
    value: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Map<String, Value>, OpenAiResponsesDecodeError> {
    object_field(value, name).ok_or_else(|| {
        OpenAiResponsesDecodeError::new(format!("Responses event omits object field {name}"))
    })
}

fn string_field<'a>(value: &'a Map<String, Value>, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn unsigned_field(value: &Map<String, Value>, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn service_tier_multiplier(model: &ModelId, service_tier: Option<&str>) -> (i128, i128) {
    match service_tier {
        Some("flex") => (1, 2),
        Some("priority") if model.as_str() == "gpt-5.5" => (5, 2),
        Some("priority") => (2, 1),
        _ => (1, 1),
    }
}

fn message_text(item: &Map<String, Value>) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        })
        .collect()
}

fn reasoning_text(item: &Map<String, Value>) -> String {
    let collect = |name: &str| {
        item.get(name)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter_map(|part| part.get("text"))
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let summary = collect("summary");
    if summary.is_empty() {
        collect("content")
    } else {
        summary
    }
}

fn nullable_display(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) => "null".into(),
        None => "undefined".into(),
        Some(value) => value.to_string(),
    }
}

fn sse_data_value(event: &str) -> Option<String> {
    let mut values = Vec::new();
    for line in event.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            values.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (!values.is_empty()).then(|| values.join("\n"))
}

fn find_sse_boundary(buffer: &[u8], codex: bool) -> Option<(usize, usize)> {
    if codex {
        return buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|position| (position, 2));
    }
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2))
        })
}

#[allow(dead_code)]
fn _typed_replay_values_are_exhaustive(
    phase: OpenAiMessagePhase,
    item_type: OpenAiToolItemType,
) -> (OpenAiMessagePhase, OpenAiToolItemType) {
    (phase, item_type)
}
