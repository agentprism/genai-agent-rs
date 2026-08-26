//! Incremental SSE decoding for the native pi-messages event protocol.

use agentprism_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantFinish, AssistantFinishReason,
    AssistantMessageDiagnostic, CancellationReason, ContentBlockId, ContentBlockKind, Currency,
    MessageId, ModelId, PI_MESSAGES_REDACTED_THINKING_KIND, PI_MESSAGES_TEXT_SIGNATURE_KIND,
    PI_MESSAGES_THINKING_SIGNATURE_KIND, PI_MESSAGES_VISIBLE_THINKING_KIND, ProviderId,
    PublicError, ReplayApplicability, ReplayDataOperation, ReplayItemId, ReplayKind, ReplayTarget,
    Timestamp, ToolCallId, TransportError, Usage, UsageSource, trim_ecmascript,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Stable identity inputs for a pi-messages decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiMessagesDecodeContext {
    /// Stable canonical message identifier.
    pub message_id: MessageId,
    /// Gateway provider identifier.
    pub provider: ProviderId,
    /// Requested model identifier.
    pub requested_model: ModelId,
    /// Timestamp retained on the assembled message.
    pub timestamp: Timestamp,
}

/// A malformed or inconsistent pi-messages stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiMessagesDecodeError {
    message: String,
}

impl PiMessagesDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PiMessagesDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PiMessagesDecodeError {}

/// Decodes a complete pi-messages SSE transcript.
pub fn decode_pi_messages_sse(
    body: &[u8],
    context: PiMessagesDecodeContext,
) -> Vec<AssistantEvent> {
    let mut decoder = PiMessagesSseDecoder::new(context);
    let mut events = decoder.take_events();
    events.extend(decoder.push(body));
    events.extend(decoder.finish());
    events
}

/// Incremental pi-messages SSE decoder.
pub struct PiMessagesSseDecoder {
    state: DecodeState,
    buffer: Vec<u8>,
    terminated: bool,
}

impl PiMessagesSseDecoder {
    /// Creates a decoder with `MessageStarted` queued.
    pub fn new(context: PiMessagesDecodeContext) -> Self {
        Self {
            state: DecodeState::new(context),
            buffer: Vec::new(),
            terminated: false,
        }
    }

    /// Drains newly emitted events.
    pub fn take_events(&mut self) -> Vec<AssistantEvent> {
        std::mem::take(&mut self.state.events)
    }

    /// Seeds a redacted transport recovery diagnostic before provider body
    /// events are consumed.
    pub fn add_diagnostic(&mut self, diagnostic: AssistantMessageDiagnostic) {
        if !self.terminated {
            self.state
                .emit(AssistantEvent::DiagnosticAdded { diagnostic })
                .expect("an active pi-messages decoder accepts response diagnostics");
        }
    }

    /// Decodes complete SSE records from one body chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        normalize_crlf(&mut self.buffer);
        while let Some((end, separator)) = find_sse_boundary(&self.buffer) {
            let record = self.buffer[..end].to_vec();
            self.buffer.drain(..end + separator);
            if let Err(error) = self.state.decode_record(&record) {
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

    /// Commits a failure when the body ends without a terminal event.
    pub fn finish(&mut self) -> Vec<AssistantEvent> {
        if self.terminated {
            return Vec::new();
        }
        normalize_crlf(&mut self.buffer);
        let has_data = std::str::from_utf8(&self.buffer)
            .map_or(true, |value| !trim_ecmascript(value).is_empty());
        if has_data {
            let record = std::mem::take(&mut self.buffer);
            if let Err(error) = self.state.decode_record(&record) {
                self.state.fail(error.to_string());
                self.terminated = true;
                return self.take_events();
            }
        }
        if self.state.assembler.is_some() {
            self.state
                .fail("pi-messages stream ended without a terminal event".into());
        }
        self.terminated = true;
        self.take_events()
    }

    /// Commits a post-establishment transport failure.
    pub fn fail_transport(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<AssistantEvent> {
        self.fail_transport_error(TransportError::new(code, message))
    }

    /// Commits an enriched post-establishment transport failure while
    /// retaining provider metadata.
    pub fn fail_transport_error(&mut self, error: TransportError) -> Vec<AssistantEvent> {
        if !self.terminated {
            self.state.fail_public(PublicError {
                code: error.code,
                message: error.message,
                retryable: false,
                provider_code: error.provider_code,
                status: error.status,
                request_id: error.request_id.or_else(|| self.state.response_id.clone()),
            });
            self.terminated = true;
        }
        self.take_events()
    }

    /// Commits caller cancellation with partial content.
    pub fn cancel(&mut self, message: impl Into<String>) -> Vec<AssistantEvent> {
        if !self.terminated {
            self.state.cancel(message.into());
            self.terminated = true;
        }
        self.take_events()
    }

    /// Whether a terminal event has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

struct DecodeState {
    context: PiMessagesDecodeContext,
    assembler: Option<AssistantAssembler>,
    events: Vec<AssistantEvent>,
    blocks: HashMap<u32, ContentBlockId>,
    finished: HashMap<u32, bool>,
    response_id: Option<String>,
    terminal_cost_micros: Option<i128>,
}

impl DecodeState {
    fn new(context: PiMessagesDecodeContext) -> Self {
        let mut state = Self {
            assembler: Some(AssistantAssembler::with_timestamp(context.timestamp)),
            events: Vec::new(),
            blocks: HashMap::new(),
            finished: HashMap::new(),
            response_id: None,
            terminal_cost_micros: None,
            context,
        };
        state
            .emit(AssistantEvent::MessageStarted {
                message_id: state.context.message_id.clone(),
                provider: state.context.provider.clone(),
                api: ApiId::new("pi-messages"),
                model: state.context.requested_model.clone(),
            })
            .expect("fresh pi-messages assembler accepts MessageStarted");
        state
    }

    fn decode_record(&mut self, record: &[u8]) -> Result<(), PiMessagesDecodeError> {
        let source = std::str::from_utf8(record).map_err(|error| {
            PiMessagesDecodeError::new(format!("SSE body is not UTF-8: {error}"))
        })?;
        let Some(data) = sse_data_value(source) else {
            return Ok(());
        };
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data).map_err(|error| {
            PiMessagesDecodeError::new(format!("invalid pi-messages SSE JSON: {error}"))
        })?;
        let event = value
            .as_object()
            .ok_or_else(|| PiMessagesDecodeError::new("pi-messages event is not an object"))?;
        self.decode_event(event)
    }

    fn decode_event(&mut self, event: &Map<String, Value>) -> Result<(), PiMessagesDecodeError> {
        let kind = string(event, "type")?;
        match kind {
            "start" => Ok(()),
            "text_start" => self.start(index(event)?, ContentBlockKind::Text),
            "text_delta" => {
                let block_id = self.block(index(event)?)?;
                self.emit(AssistantEvent::TextDelta {
                    block_id,
                    delta: string(event, "delta")?.to_owned(),
                })
            }
            "text_end" => {
                let content_index = index(event)?;
                let block_id = self.block(content_index)?;
                self.emit(AssistantEvent::TextReplaced {
                    block_id: block_id.clone(),
                    text: string(event, "content")?.to_owned(),
                })?;
                if let Some(signature) = event.get("contentSignature").and_then(Value::as_str) {
                    self.replay_utf8(
                        content_index.saturating_mul(2),
                        &block_id,
                        PI_MESSAGES_TEXT_SIGNATURE_KIND,
                        signature,
                    )?;
                }
                self.end(content_index, block_id)
            }
            "thinking_start" => self.start(index(event)?, ContentBlockKind::Thinking),
            "thinking_delta" => {
                let block_id = self.block(index(event)?)?;
                self.emit(AssistantEvent::ThinkingDelta {
                    block_id,
                    delta: string(event, "delta")?.to_owned(),
                })
            }
            "thinking_end" => {
                let content_index = index(event)?;
                let block_id = self.block(content_index)?;
                self.emit(AssistantEvent::ThinkingReplaced {
                    block_id: block_id.clone(),
                    thinking: string(event, "content")?.to_owned(),
                })?;
                if let Some(redacted) = event.get("redacted").and_then(Value::as_bool) {
                    self.replay_utf8(
                        content_index.saturating_mul(2),
                        &block_id,
                        if redacted {
                            PI_MESSAGES_REDACTED_THINKING_KIND
                        } else {
                            PI_MESSAGES_VISIBLE_THINKING_KIND
                        },
                        if redacted { "true" } else { "false" },
                    )?;
                }
                if let Some(signature) = event.get("contentSignature").and_then(Value::as_str) {
                    self.replay_utf8(
                        content_index.saturating_mul(2).saturating_add(1),
                        &block_id,
                        PI_MESSAGES_THINKING_SIGNATURE_KIND,
                        signature,
                    )?;
                }
                self.end(content_index, block_id)
            }
            "toolcall_start" => {
                let content_index = index(event)?;
                self.start(content_index, ContentBlockKind::ToolCall)?;
                let block_id = self.block(content_index)?;
                self.emit(AssistantEvent::ToolCallMetadata {
                    block_id,
                    call_id: ToolCallId::new(string(event, "id")?),
                    name: Some(string(event, "toolName")?.to_owned()),
                })
            }
            "toolcall_delta" => {
                let block_id = self.block(index(event)?)?;
                self.emit(AssistantEvent::ToolArgumentsDelta {
                    block_id,
                    delta: string(event, "delta")?.to_owned(),
                })
            }
            "toolcall_end" => {
                let content_index = index(event)?;
                let block_id = self.block(content_index)?;
                if let Some(tool_call) = event.get("toolCall").and_then(Value::as_object) {
                    self.emit(AssistantEvent::ToolCallMetadataReplaced {
                        block_id: block_id.clone(),
                        call_id: ToolCallId::new(string(tool_call, "id")?),
                        name: string(tool_call, "name")?.to_owned(),
                    })?;
                    let arguments = tool_call.get("arguments").ok_or_else(|| {
                        PiMessagesDecodeError::new("toolCall.arguments is required")
                    })?;
                    self.emit(AssistantEvent::ToolArgumentsReplaced {
                        block_id: block_id.clone(),
                        arguments: serde_json::to_string(arguments).map_err(|error| {
                            PiMessagesDecodeError::new(format!(
                                "could not preserve tool arguments: {error}"
                            ))
                        })?,
                    })?;
                }
                self.end(content_index, block_id)
            }
            "done" => self.complete(event),
            "error" => self.server_error(event),
            other => Err(PiMessagesDecodeError::new(format!(
                "unknown pi-messages event type {other}"
            ))),
        }
    }

    fn replay_utf8(
        &mut self,
        ordinal: u32,
        block_id: &ContentBlockId,
        kind: &str,
        value: &str,
    ) -> Result<(), PiMessagesDecodeError> {
        let item_id = ReplayItemId::new(format!("pi-messages-replay-{ordinal}"));
        self.emit(AssistantEvent::ReplayItemStarted {
            item_id: item_id.clone(),
            ordinal,
            target: ReplayTarget::ContentBlock(block_id.clone()),
            kind: ReplayKind::new(kind),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })?;
        self.emit(AssistantEvent::ReplayData {
            item_id: item_id.clone(),
            operation: ReplayDataOperation::ReplaceUtf8(value.to_owned()),
        })?;
        self.emit(AssistantEvent::ReplayItemFinished { item_id })
    }

    fn start(
        &mut self,
        content_index: u32,
        kind: ContentBlockKind,
    ) -> Result<(), PiMessagesDecodeError> {
        let block_id = ContentBlockId::new(format!(
            "pi-messages-block-{}-{content_index}",
            self.context.message_id
        ));
        self.blocks.insert(content_index, block_id.clone());
        self.finished.insert(content_index, false);
        self.emit(AssistantEvent::ContentBlockStarted {
            block_id,
            content_index,
            kind,
        })
    }

    fn end(
        &mut self,
        content_index: u32,
        block_id: ContentBlockId,
    ) -> Result<(), PiMessagesDecodeError> {
        self.emit(AssistantEvent::ContentBlockFinished { block_id })?;
        self.finished.insert(content_index, true);
        Ok(())
    }

    fn block(&self, content_index: u32) -> Result<ContentBlockId, PiMessagesDecodeError> {
        self.blocks.get(&content_index).cloned().ok_or_else(|| {
            PiMessagesDecodeError::new(format!("event references unopened block {content_index}"))
        })
    }

    fn complete(&mut self, event: &Map<String, Value>) -> Result<(), PiMessagesDecodeError> {
        self.update_terminal_metadata(event)?;
        self.append_rewrite_diagnostic(event)?;
        let reason = match string(event, "reason")? {
            "stop" => AssistantFinishReason::Stop,
            "length" => AssistantFinishReason::Length,
            "toolUse" => AssistantFinishReason::ToolUse,
            other => {
                return Err(PiMessagesDecodeError::new(format!(
                    "invalid successful stop reason {other}"
                )));
            }
        };
        let assembler = self
            .assembler
            .take()
            .ok_or_else(|| PiMessagesDecodeError::new("pi-messages stream already terminated"))?;
        let mut message = assembler
            .finish_completed(AssistantFinish {
                reason,
                raw_provider_reason: None,
                error: None,
            })
            .map_err(|error| {
                PiMessagesDecodeError::new(format!("invalid completed pi-messages stream: {error}"))
            })?;
        self.apply_cost(&mut message);
        self.events.push(AssistantEvent::Finished { message });
        Ok(())
    }

    fn server_error(&mut self, event: &Map<String, Value>) -> Result<(), PiMessagesDecodeError> {
        self.update_terminal_metadata(event)?;
        self.append_rewrite_diagnostic(event)?;
        let reason = string(event, "reason")?;
        let message = event
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or("pi-messages upstream failed")
            .to_owned();
        if reason == "aborted" {
            self.cancel(message);
        } else if reason == "error" {
            self.fail_public(PublicError {
                code: "provider_error".into(),
                message,
                retryable: false,
                provider_code: None,
                status: None,
                request_id: self.response_id.clone(),
            });
        } else {
            return Err(PiMessagesDecodeError::new(format!(
                "invalid error stop reason {reason}"
            )));
        }
        Ok(())
    }

    fn update_terminal_metadata(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), PiMessagesDecodeError> {
        if let Some(response_id) = event.get("responseId").and_then(Value::as_str) {
            self.response_id = Some(response_id.to_owned());
            self.emit(AssistantEvent::ResponseMetadata {
                response_id: Some(response_id.to_owned()),
                response_model: None,
                end_turn: None,
            })?;
        }
        let usage = parse_usage(event.get("usage"))?;
        self.terminal_cost_micros = parse_total_cost_micros(event.get("usage"));
        self.emit(AssistantEvent::UsageUpdated { cumulative: usage })
    }

    fn append_rewrite_diagnostic(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<(), PiMessagesDecodeError> {
        let Some(rewrite) = event.get("rewrite") else {
            return Ok(());
        };
        let details = rewrite
            .as_object()
            .ok_or_else(|| PiMessagesDecodeError::new("pi-messages rewrite is not an object"))?
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        self.emit(AssistantEvent::DiagnosticAdded {
            diagnostic: AssistantMessageDiagnostic {
                schema_version: agentprism_ai::ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
                kind: "pi_messages_rewrite".into(),
                timestamp: now(),
                error: None,
                details,
            },
        })
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
        let mut message = assembler.finish_failed(error, None);
        self.apply_cost(&mut message);
        self.events.push(AssistantEvent::Failed { message });
    }

    fn cancel(&mut self, message: String) {
        let Some(assembler) = self.assembler.take() else {
            return;
        };
        let mut message = assembler.finish_cancelled(CancellationReason {
            message,
            request_id: self.response_id.clone(),
        });
        self.apply_cost(&mut message);
        self.events.push(AssistantEvent::Cancelled { message });
    }

    fn apply_cost(&self, message: &mut agentprism_ai::AssistantMessage) {
        if let Some(micros) = self.terminal_cost_micros {
            message.cost = Some(agentprism_ai::Cost {
                currency: Currency::usd(),
                micros,
            });
        }
    }

    fn emit(&mut self, event: AssistantEvent) -> Result<(), PiMessagesDecodeError> {
        self.assembler
            .as_mut()
            .ok_or_else(|| PiMessagesDecodeError::new("pi-messages decoder already terminated"))?
            .apply(&event)
            .map_err(|error| {
                PiMessagesDecodeError::new(format!("invalid decoded event: {error}"))
            })?;
        self.events.push(event);
        Ok(())
    }
}

fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn parse_usage(value: Option<&Value>) -> Result<Usage, PiMessagesDecodeError> {
    let usage = value
        .and_then(Value::as_object)
        .ok_or_else(|| PiMessagesDecodeError::new("pi-messages terminal event omits usage"))?;
    Ok(Usage {
        input_tokens: number(usage, "input"),
        output_tokens: number(usage, "output"),
        reasoning_tokens: None,
        cache_read_tokens: Some(number(usage, "cacheRead")),
        cache_write_tokens: Some(number(usage, "cacheWrite")),
        cache_write_one_hour_tokens: None,
        total_tokens: usage.get("totalTokens").and_then(Value::as_u64),
        source: UsageSource::ProviderReported,
    })
}

fn parse_total_cost_micros(value: Option<&Value>) -> Option<i128> {
    let number = value?.get("cost")?.get("total")?.as_number()?.to_string();
    decimal_micros(&number)
}

fn decimal_micros(value: &str) -> Option<i128> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |rest| (true, rest));
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = whole.parse::<i128>().ok()?;
    let mut fraction = fraction.chars().take(6).collect::<String>();
    while fraction.len() < 6 {
        fraction.push('0');
    }
    let micros = whole
        .checked_mul(1_000_000)?
        .checked_add(fraction.parse::<i128>().unwrap_or(0))?;
    Some(if negative { -micros } else { micros })
}

fn index(event: &Map<String, Value>) -> Result<u32, PiMessagesDecodeError> {
    event
        .get("contentIndex")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| PiMessagesDecodeError::new("event omits a valid contentIndex"))
}

fn string<'a>(
    event: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, PiMessagesDecodeError> {
    event
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| PiMessagesDecodeError::new(format!("event omits string {field}")))
}

fn number(event: &Map<String, Value>, field: &str) -> u64 {
    event.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
}

fn normalize_crlf(buffer: &mut Vec<u8>) {
    let mut read = 0;
    let mut write = 0;
    while read < buffer.len() {
        if buffer.get(read..read + 2) == Some(b"\r\n") {
            buffer[write] = b'\n';
            read += 2;
        } else {
            buffer[write] = buffer[read];
            read += 1;
        }
        write += 1;
    }
    buffer.truncate(write);
}

fn sse_data_value(source: &str) -> Option<String> {
    let normalized = source.replace("\r\n", "\n");
    normalized
        .split('\n')
        .find_map(|line| line.strip_prefix("data:").map(trim_ecmascript))
        .filter(|data| !data.is_empty())
        .map(str::to_owned)
}
