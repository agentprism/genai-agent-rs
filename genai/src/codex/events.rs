//! Codex Responses event stream → genai [`ChatStreamEvent`] translation.
//!
//! The ChatGPT Codex backend speaks the OpenAI **Responses** event vocabulary
//! (identical over SSE frames and WebSocket frames). Rather than re-implement the
//! assistant-event bookkeeping, this crate translates each Codex event into the
//! same [`ChatStreamEvent`] vocabulary that genai's own OpenAI-Responses adapter
//! emits, then folds it through rust-genai-agent's [`AssistantAccumulator`] — the
//! *exact* path [`GenaiStreamFn`] uses. That guarantees `CodexStreamFn` produces
//! the identical `AssistantMessageEventStream` contract (start / text deltas /
//! thinking / tool calls / done / error) as every other stream function.
//!
//! Ported from pi-ai's `mapCodexEvents` (openai-codex-responses.ts:722-758) and
//! `processResponsesStream` (openai-responses-shared.ts:416-740); line citations
//! inline. Terminal decisions (success vs in-band error) follow pi's
//! `mapStopReason` (openai-responses-shared.ts:742-772) and `assertSuccessfulOutput`
//! (openai-codex-responses.ts:117-124).
//!
//! [`GenaiStreamFn`]: genai::GenaiStreamFn
//! [`AssistantAccumulator`]: genai::AssistantAccumulator

use std::collections::HashMap;

use genai::chat::{
    ChatStreamEvent, CompletionTokensDetails, PromptTokensDetails, StopReason as GenaiStopReason,
    StreamChunk, StreamEnd, ToolCall, ToolChunk, Usage,
};
use serde_json::Value;

/// In-band `error.code` that signals the WS connection pool is exhausted
/// (`WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE`, openai-codex-responses.ts:70). This
/// is the *only* in-band error that pi treats as a pre-commit transport failure
/// eligible for SSE fallback (:349, :701-703); every other `error` /
/// `response.failed` frame is a terminal in-band error.
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";

/// One translated unit to feed the accumulator.
pub enum MappedItem {
    /// Fold this genai stream event (`ChatStreamEvent::End` is terminal → `Done`).
    Stream(ChatStreamEvent),
    /// An application-level (non-transport) error → in-band terminal `Error`.
    Fail(String),
    /// A pre-commit **transport** failure delivered in-band as
    /// `{"type":"error","code":"websocket_connection_limit_reached"}`
    /// (openai-codex-responses.ts:701-703). Over WebSocket, before any content is
    /// committed, the caller falls back to SSE (pi's :349-378). After commit — or
    /// on the SSE path where there is nothing left to fall back to — the caller
    /// treats it as a terminal in-band error (pi re-throws once started, :373).
    TransportFail(String),
}

/// Per-`output_index` scratch state for an in-flight tool call.
struct ToolSlot {
    /// Composite `callId|itemId` id (openai-responses-shared.ts:472).
    id: String,
    name: String,
    /// Cumulative raw argument JSON observed so far.
    args: String,
}

/// Stateful translator from Codex Responses events to [`MappedItem`]s.
///
/// Streamed content is tracked **per `output_index`** (matching pi's per-slot
/// bookkeeping, openai-responses-shared.ts:662-689) so a second message/reasoning
/// block that arrives only via `response.output_item.done` is not dropped.
#[derive(Default)]
pub struct CodexEventMapper {
    slots: HashMap<u64, ToolSlot>,
    /// Text streamed so far for each message `output_index` (empty until its first
    /// text delta; present-and-empty is never inserted for text).
    text_streamed: HashMap<u64, String>,
    /// Reasoning streamed so far for each reasoning `output_index`. Presence of a
    /// key means a reasoning slot exists for that index (pi's `getSlot`, :585).
    reasoning_streamed: HashMap<u64, String>,
    /// `response.created`'s `response.id`, used as the fallback terminal response id
    /// when the terminal event omits `id` (openai-responses-shared.ts:581, 538-539).
    response_id: Option<String>,
}

impl CodexEventMapper {
    /// New empty mapper.
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one Codex event value into zero or more [`MappedItem`]s.
    pub fn map(&mut self, event: &Value) -> Vec<MappedItem> {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };

        match kind {
            // -- Codex-specific normalization (mapCodexEvents) --
            // An in-band `error` frame is normally a terminal application error,
            // EXCEPT `websocket_connection_limit_reached`, which pi treats as a
            // pre-commit transport failure eligible for SSE fallback (:349, :701-703).
            "error" => {
                let message = codex_error_message(event);
                if codex_error_code(event).as_deref()
                    == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE)
                {
                    vec![MappedItem::TransportFail(message)]
                } else {
                    vec![MappedItem::Fail(message)]
                }
            }
            "response.failed" => vec![MappedItem::Fail(response_failed_message(event))],
            "response.done" | "response.completed" | "response.incomplete" => {
                vec![self.terminal(event)]
            }

            // -- Streaming content (processResponsesStream) --
            // First frame commits the transport and emits the assistant `start`.
            // pi also records `response.id` here (shared:581) as the fallback id.
            "response.created" => {
                if let Some(id) = event
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                {
                    self.response_id = Some(id.to_string());
                }
                vec![MappedItem::Stream(ChatStreamEvent::Start)]
            }
            "response.output_item.added" => self.output_item_added(event),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.reasoning_delta(event)
            }
            "response.reasoning_summary_part.done" => self.reasoning_part_done(event),
            "response.output_text.delta" | "response.refusal.delta" => self.text_delta(event),
            "response.function_call_arguments.delta" => self.fn_args_delta(event),
            "response.function_call_arguments.done" => self.fn_args_done(event),
            "response.custom_tool_call_input.delta" => self.custom_input_delta(event),
            "response.custom_tool_call_input.done" => self.custom_input_done(event),
            "response.output_item.done" => self.output_item_done(event),

            // response.created's response.id is captured at terminal instead;
            // unknown events are ignored (forward-compatible, matches pi's else).
            _ => Vec::new(),
        }
    }

    /// Build the terminal item from a `response.{completed,done,incomplete}` event.
    fn terminal(&self, event: &Value) -> MappedItem {
        let response = event.get("response");
        let status = response
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str);
        let incomplete_reason = response
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str);

        // Success vs in-band error, mirroring pi's mapStopReason + assertSuccessfulOutput.
        let captured_stop_reason: Option<GenaiStopReason> = match status {
            Some("incomplete") => {
                if incomplete_reason == Some("max_output_tokens") {
                    Some(GenaiStopReason::MaxTokens(
                        "incomplete.max_output_tokens".to_string(),
                    ))
                } else {
                    let message = match incomplete_reason {
                        Some(reason) => format!("Response incomplete: {reason}"),
                        None => "Response incomplete without a provider reason".to_string(),
                    };
                    return MappedItem::Fail(message);
                }
            }
            Some("failed") | Some("cancelled") => {
                // pi has no error message here; assertSuccessfulOutput uses this fallback.
                return MappedItem::Fail("An unknown error occurred".to_string());
            }
            // completed / in_progress / queued / unknown / missing: let the
            // accumulator infer Stop vs ToolUse from the captured tool calls
            // (mapStopReason(undefined, hasToolCalls)).
            _ => None,
        };

        let end = StreamEnd {
            captured_usage: response.and_then(|r| r.get("usage")).and_then(parse_usage),
            captured_stop_reason,
            captured_content: None,
            captured_reasoning_content: None,
            captured_response_id: response
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                // Fall back to the id captured from `response.created` when the
                // terminal event omits it (openai-responses-shared.ts:538-539).
                .or_else(|| self.response_id.clone()),
        };
        MappedItem::Stream(ChatStreamEvent::End(end))
    }

    fn output_item_added(&mut self, event: &Value) -> Vec<MappedItem> {
        let Some(item) = event.get("item") else {
            return Vec::new();
        };
        let output_index = output_index(event);
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let id = composite_tool_id(item);
                let name = str_field(item, "name");
                let args = str_field(item, "arguments");
                self.slots.insert(
                    output_index,
                    ToolSlot {
                        id: id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                    },
                );
                vec![MappedItem::Stream(tool_chunk(id, name, args))]
            }
            Some("custom_tool_call") => {
                let id = composite_tool_id(item);
                let name = str_field(item, "name");
                let input = str_field(item, "input");
                self.slots.insert(
                    output_index,
                    ToolSlot {
                        id: id.clone(),
                        name: name.clone(),
                        args: input.clone(),
                    },
                );
                vec![MappedItem::Stream(tool_chunk(id, name, input))]
            }
            // reasoning / message blocks open lazily on their first delta.
            _ => Vec::new(),
        }
    }

    fn reasoning_delta(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        match event.get("delta").and_then(Value::as_str) {
            Some(delta) => {
                self.reasoning_streamed
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                vec![MappedItem::Stream(reasoning_chunk(delta))]
            }
            None => Vec::new(),
        }
    }

    /// `response.reasoning_summary_part.done` → append a `"\n\n"` separator, but
    /// ONLY when a reasoning slot already exists for this `output_index` (pi's
    /// `if (!slot) continue;`, openai-responses-shared.ts:594-596). This stops a
    /// stray `part.done` from materializing an empty thinking block.
    fn reasoning_part_done(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        match self.reasoning_streamed.get_mut(&output_index) {
            Some(streamed) => {
                streamed.push_str("\n\n");
                vec![MappedItem::Stream(reasoning_chunk("\n\n"))]
            }
            None => Vec::new(),
        }
    }

    fn text_delta(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        match event.get("delta").and_then(Value::as_str) {
            Some(delta) => {
                self.text_streamed
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                vec![MappedItem::Stream(text_chunk(delta))]
            }
            None => Vec::new(),
        }
    }

    fn fn_args_delta(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        let Some(delta) = event.get("delta").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(slot) = self.slots.get_mut(&output_index) else {
            return Vec::new();
        };
        slot.args.push_str(delta);
        vec![MappedItem::Stream(tool_chunk(
            slot.id.clone(),
            slot.name.clone(),
            slot.args.clone(),
        ))]
    }

    fn fn_args_done(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        let Some(args) = event.get("arguments").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(slot) = self.slots.get_mut(&output_index) else {
            return Vec::new();
        };
        slot.args = args.to_string();
        vec![MappedItem::Stream(tool_chunk(
            slot.id.clone(),
            slot.name.clone(),
            slot.args.clone(),
        ))]
    }

    fn custom_input_delta(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        let Some(delta) = event.get("delta").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(slot) = self.slots.get_mut(&output_index) else {
            return Vec::new();
        };
        slot.args.push_str(delta);
        vec![MappedItem::Stream(tool_chunk(
            slot.id.clone(),
            slot.name.clone(),
            slot.args.clone(),
        ))]
    }

    fn custom_input_done(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        let Some(input) = event.get("input").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(slot) = self.slots.get_mut(&output_index) else {
            return Vec::new();
        };
        slot.args = input.to_string();
        vec![MappedItem::Stream(tool_chunk(
            slot.id.clone(),
            slot.name.clone(),
            slot.args.clone(),
        ))]
    }

    fn output_item_done(&mut self, event: &Value) -> Vec<MappedItem> {
        let output_index = output_index(event);
        let Some(item) = event.get("item") else {
            return Vec::new();
        };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let (id, name) = match self.slots.remove(&output_index) {
                    Some(slot) => (slot.id, slot.name),
                    None => (composite_tool_id(item), str_field(item, "name")),
                };
                let args = str_field(item, "arguments");
                vec![MappedItem::Stream(tool_chunk(id, name, args))]
            }
            Some("custom_tool_call") => {
                let (id, name) = match self.slots.remove(&output_index) {
                    Some(slot) => (slot.id, slot.name),
                    None => (composite_tool_id(item), str_field(item, "name")),
                };
                let input = str_field(item, "input");
                vec![MappedItem::Stream(tool_chunk(id, name, input))]
            }
            // Reconcile this slot's text/reasoning with the item's authoritative
            // content, INDEPENDENTLY per `output_index` (pi sets the block to the
            // authoritative content on done, openai-responses-shared.ts:680-689 /
            // 667-679). Because the accumulator concatenates deltas into a single
            // block, emit only the suffix of the authoritative text beyond what
            // already streamed for this slot: this backfills a block that never
            // streamed (fixing dropped multi-block content) and appends a backend
            // correction that extends the streamed prefix, without double-emitting.
            Some("message") => {
                let authoritative = message_output_text(item);
                let streamed = self.text_streamed.entry(output_index).or_default();
                reconcile_done(streamed, authoritative)
                    .map(|tail| vec![MappedItem::Stream(text_chunk(&tail))])
                    .unwrap_or_default()
            }
            Some("reasoning") => {
                let authoritative = reasoning_output_text(item);
                let streamed = self.reasoning_streamed.entry(output_index).or_default();
                reconcile_done(streamed, authoritative)
                    .map(|tail| vec![MappedItem::Stream(reasoning_chunk(&tail))])
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }
}

// -- event field helpers ----------------------------------------------------

fn output_index(event: &Value) -> u64 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Composite `callId|itemId` (openai-responses-shared.ts:472). When the item id
/// is absent/empty the bare `call_id` is used.
fn composite_tool_id(item: &Value) -> String {
    let call_id = str_field(item, "call_id");
    let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    if item_id.is_empty() {
        call_id
    } else {
        format!("{call_id}|{item_id}")
    }
}

fn message_output_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("refusal").and_then(Value::as_str))
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn reasoning_output_text(item: &Value) -> String {
    let join = |key: &str| -> Option<String> {
        item.get(key).and_then(Value::as_array).map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
    };
    let summary = join("summary").filter(|s| !s.is_empty());
    let content = join("content").filter(|s| !s.is_empty());
    summary.or(content).unwrap_or_default()
}

// -- ChatStreamEvent constructors -------------------------------------------

fn text_chunk(content: &str) -> ChatStreamEvent {
    ChatStreamEvent::Chunk(StreamChunk {
        content: content.to_string(),
    })
}

fn reasoning_chunk(content: &str) -> ChatStreamEvent {
    ChatStreamEvent::ReasoningChunk(StreamChunk {
        content: content.to_string(),
    })
}

fn tool_chunk(id: String, name: String, raw_args: String) -> ChatStreamEvent {
    ChatStreamEvent::ToolCallChunk(ToolChunk {
        tool_call: ToolCall {
            call_id: id,
            fn_name: name,
            // The accumulator treats a String value as cumulative raw JSON.
            fn_arguments: Value::String(raw_args),
            thought_signatures: None,
        },
    })
}

// -- output_item.done reconciliation ----------------------------------------

/// Reconcile a slot's already-streamed text with the authoritative text from a
/// `response.output_item.done` item, returning the suffix to emit (if any) and
/// recording the new streamed value.
///
/// The genai accumulator concatenates chunks into a single block, so it cannot
/// *replace* text; this converges on the authoritative content for the realistic
/// cases while never double-emitting:
/// - empty authoritative → keep the streamed text (pi's `... || slot.block.text`
///   for reasoning, openai-responses-shared.ts:670; the message contract always
///   carries the full text, so an empty item after streaming is treated the same);
/// - nothing streamed yet → emit the whole authoritative text (backfill);
/// - authoritative extends the streamed prefix → emit only the appended tail
///   (mirrors pi's `startsWith` slice for tool args, :647-649);
/// - authoritative equals the streamed text, or diverges in a way a concatenating
///   accumulator cannot represent → emit nothing (keep the streamed text).
fn reconcile_done(streamed: &mut String, authoritative: String) -> Option<String> {
    if authoritative.is_empty() {
        return None;
    }
    if streamed.is_empty() {
        *streamed = authoritative.clone();
        return Some(authoritative);
    }
    if authoritative.len() > streamed.len() && authoritative.starts_with(streamed.as_str()) {
        let tail = authoritative[streamed.len()..].to_string();
        *streamed = authoritative;
        return Some(tail);
    }
    None
}

// -- error messages (mapCodexEvents) ----------------------------------------

/// The error `code`, top-level or nested under `error`, if present
/// (`extractCodexEventError`, openai-codex-responses.ts:709-720).
fn codex_error_code(event: &Value) -> Option<String> {
    let nested = event.get("error");
    event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| nested.and_then(|e| e.get("code")).and_then(Value::as_str))
        .map(str::to_string)
}

/// `Codex error: {message || code || JSON}` (openai-codex-responses.ts:709-733).
fn codex_error_message(event: &Value) -> String {
    let nested = event.get("error");
    let message = event.get("message").and_then(Value::as_str).or_else(|| {
        nested
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    });
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| nested.and_then(|e| e.get("code")).and_then(Value::as_str));
    let detail = message
        .map(str::to_string)
        .or_else(|| code.map(str::to_string))
        .unwrap_or_else(|| event.to_string());
    format!("Codex error: {detail}")
}

/// `response.error.message || "Codex response failed"` (openai-codex-responses.ts:735-740).
fn response_failed_message(event: &Value) -> String {
    event
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "Codex response failed".to_string())
}

// -- usage ------------------------------------------------------------------

/// Parse the Codex `response.usage` object into genai [`Usage`]
/// (openai-codex-responses/openai-responses-shared finalizeResponse:541-557).
///
/// CONVENTION: unlike pi (which subtracts cached + cache-write from the input
/// count for its own `Usage.input`), this keeps OpenAI's inclusive `input_tokens`
/// as genai's `prompt_tokens`. That matches `AgentUsage::from(genai::Usage)` and
/// therefore how `GenaiStreamFn` reports usage, keeping the crate consistent:
/// `AgentUsage.input_tokens` includes cache reads, with `cache_read_tokens` /
/// `cache_write_tokens` reported separately.
fn parse_usage(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let input_details = usage.get("input_tokens_details");
    let cached = detail_i32(input_details, "cached_tokens");
    let cache_write = detail_i32(input_details, "cache_write_tokens");
    let reasoning = detail_i32(usage.get("output_tokens_details"), "reasoning_tokens");

    let prompt_tokens_details = if cached.is_some() || cache_write.is_some() {
        Some(PromptTokensDetails {
            cached_tokens: cached,
            cache_creation_tokens: cache_write,
            ..Default::default()
        })
    } else {
        None
    };
    let completion_tokens_details = reasoning.map(|tokens| CompletionTokensDetails {
        reasoning_tokens: Some(tokens),
        ..Default::default()
    });

    Some(Usage {
        prompt_tokens: as_i32(usage, "input_tokens"),
        prompt_tokens_details,
        completion_tokens: as_i32(usage, "output_tokens"),
        completion_tokens_details,
        total_tokens: as_i32(usage, "total_tokens"),
    })
}

fn as_i32(value: &Value, key: &str) -> Option<i32> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .map(|n| n.clamp(0, i32::MAX as i64) as i32)
}

fn detail_i32(details: Option<&Value>, key: &str) -> Option<i32> {
    details
        .and_then(|d| d.get(key))
        .and_then(Value::as_i64)
        .map(|n| n.clamp(0, i32::MAX as i64) as i32)
        .filter(|n| *n > 0)
}

/// A minimal SSE frame decoder.
///
/// Buffers raw bytes and yields the JSON payload of each complete event (events
/// separated by a blank line), joining `data:` lines and skipping `[DONE]`.
/// Byte-buffered so a multi-byte UTF-8 sequence split across network chunks is
/// never mis-decoded (events split only at the ASCII `\n\n` boundary). Port of
/// pi's `parseSSE` (openai-codex-responses.ts:764-821).
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    /// New empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; return the JSON text of every newly completed event.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = find_double_newline(&self.buffer) {
            let frame = self.buffer.drain(..pos + 2).collect::<Vec<u8>>();
            let frame = String::from_utf8_lossy(&frame[..pos]);
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                events.push(data.to_string());
            }
        }
        events
    }
}

fn find_double_newline(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map_all(mapper: &mut CodexEventMapper, event: Value) -> Vec<MappedItem> {
        mapper.map(&event)
    }

    #[test]
    fn text_delta_maps_to_chunk() {
        let mut mapper = CodexEventMapper::new();
        let items = map_all(
            &mut mapper,
            json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "hi" }),
        );
        assert_eq!(items.len(), 1);
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::Chunk(chunk)) => assert_eq!(chunk.content, "hi"),
            _ => panic!("expected text chunk"),
        }
    }

    #[test]
    fn reasoning_deltas_and_part_done() {
        let mut mapper = CodexEventMapper::new();
        assert!(matches!(
            map_all(
                &mut mapper,
                json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "think" })
            )[0],
            MappedItem::Stream(ChatStreamEvent::ReasoningChunk(_))
        ));
        match &map_all(
            &mut mapper,
            json!({ "type": "response.reasoning_summary_part.done", "output_index": 0 }),
        )[0]
        {
            MappedItem::Stream(ChatStreamEvent::ReasoningChunk(c)) => assert_eq!(c.content, "\n\n"),
            _ => panic!("expected reasoning chunk"),
        }
    }

    #[test]
    fn function_call_lifecycle_accumulates_args() {
        let mut mapper = CodexEventMapper::new();
        map_all(
            &mut mapper,
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "function_call", "call_id": "c1", "id": "fc_1", "name": "f", "arguments": "" }
            }),
        );
        let items = map_all(
            &mut mapper,
            json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"a\":" }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::ToolCallChunk(chunk)) => {
                assert_eq!(chunk.tool_call.call_id, "c1|fc_1");
                assert_eq!(chunk.tool_call.fn_name, "f");
                assert_eq!(chunk.tool_call.fn_arguments, json!("{\"a\":"));
            }
            _ => panic!("expected tool chunk"),
        }
        let done = map_all(
            &mut mapper,
            json!({ "type": "response.function_call_arguments.done", "output_index": 0, "arguments": "{\"a\":1}" }),
        );
        match &done[0] {
            MappedItem::Stream(ChatStreamEvent::ToolCallChunk(chunk)) => {
                assert_eq!(chunk.tool_call.fn_arguments, json!("{\"a\":1}"));
            }
            _ => panic!("expected tool chunk"),
        }
    }

    #[test]
    fn terminal_completed_builds_end_with_usage() {
        let mut mapper = CodexEventMapper::new();
        let items = map_all(
            &mut mapper,
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "usage": {
                        "input_tokens": 100,
                        "input_tokens_details": { "cached_tokens": 10 },
                        "output_tokens": 20,
                        "output_tokens_details": { "reasoning_tokens": 5 },
                        "total_tokens": 120
                    }
                }
            }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::End(end)) => {
                assert_eq!(end.captured_response_id.as_deref(), Some("resp_1"));
                let usage = end.captured_usage.as_ref().expect("usage");
                assert_eq!(usage.prompt_tokens, Some(100));
                assert_eq!(usage.completion_tokens, Some(20));
                assert_eq!(usage.total_tokens, Some(120));
                assert_eq!(
                    usage.prompt_tokens_details.as_ref().unwrap().cached_tokens,
                    Some(10)
                );
                assert_eq!(
                    usage
                        .completion_tokens_details
                        .as_ref()
                        .unwrap()
                        .reasoning_tokens,
                    Some(5)
                );
                // completed -> let the accumulator infer the stop reason.
                assert!(end.captured_stop_reason.is_none());
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn incomplete_max_tokens_is_length_success() {
        let mut mapper = CodexEventMapper::new();
        let items = map_all(
            &mut mapper,
            json!({
                "type": "response.incomplete",
                "response": { "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" } }
            }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::End(end)) => assert!(matches!(
                end.captured_stop_reason,
                Some(GenaiStopReason::MaxTokens(_))
            )),
            _ => panic!("expected End(MaxTokens)"),
        }
    }

    #[test]
    fn incomplete_other_reason_fails() {
        let mut mapper = CodexEventMapper::new();
        let items = map_all(
            &mut mapper,
            json!({
                "type": "response.incomplete",
                "response": { "status": "incomplete", "incomplete_details": { "reason": "content_filter" } }
            }),
        );
        match &items[0] {
            MappedItem::Fail(msg) => assert_eq!(msg, "Response incomplete: content_filter"),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn error_and_failed_events_fail() {
        let mut mapper = CodexEventMapper::new();
        match &map_all(
            &mut mapper,
            json!({ "type": "error", "code": "x", "message": "boom" }),
        )[0]
        {
            MappedItem::Fail(msg) => assert_eq!(msg, "Codex error: boom"),
            _ => panic!("expected Fail"),
        }
        match &map_all(
            &mut mapper,
            json!({ "type": "response.failed", "response": { "error": { "message": "nope" } } }),
        )[0]
        {
            MappedItem::Fail(msg) => assert_eq!(msg, "nope"),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn sse_decoder_splits_frames_and_skips_done() {
        let mut decoder = SseDecoder::new();
        let mut out = decoder.push(b"data: {\"type\":\"a\"}\n\ndata: {\"ty");
        assert_eq!(out, vec![r#"{"type":"a"}"#.to_string()]);
        out = decoder.push(b"pe\":\"b\"}\n\ndata: [DONE]\n\n");
        assert_eq!(out, vec![r#"{"type":"b"}"#.to_string()]);
    }

    #[test]
    fn connection_limit_error_maps_to_transport_fail() {
        // H1: the connection-limit code is a pre-commit transport failure...
        let mut mapper = CodexEventMapper::new();
        match &map_all(
            &mut mapper,
            json!({ "type": "error", "code": "websocket_connection_limit_reached", "message": "busy" }),
        )[0]
        {
            MappedItem::TransportFail(msg) => assert_eq!(msg, "Codex error: busy"),
            _ => panic!("expected TransportFail"),
        }
        // ...but any other error code stays a terminal application Fail.
        match &map_all(
            &mut mapper,
            json!({ "type": "error", "code": "server_error", "message": "boom" }),
        )[0]
        {
            MappedItem::Fail(msg) => assert_eq!(msg, "Codex error: boom"),
            _ => panic!("expected Fail"),
        }
        // A nested `error.code` is honored too (extractCodexEventError).
        match &map_all(
            &mut mapper,
            json!({ "type": "error", "error": { "code": "websocket_connection_limit_reached", "message": "busy" } }),
        )[0]
        {
            MappedItem::TransportFail(_) => {}
            _ => panic!("expected TransportFail from nested code"),
        }
    }

    #[test]
    fn reasoning_part_done_without_slot_is_dropped() {
        // L8: a stray part.done with no prior reasoning slot emits nothing.
        let mut mapper = CodexEventMapper::new();
        assert!(
            map_all(
                &mut mapper,
                json!({ "type": "response.reasoning_summary_part.done", "output_index": 0 }),
            )
            .is_empty()
        );
    }

    #[test]
    fn response_id_falls_back_to_created() {
        // L6: capture response.created's id and use it when the terminal omits it.
        let mut mapper = CodexEventMapper::new();
        let _ = map_all(
            &mut mapper,
            json!({ "type": "response.created", "response": { "id": "resp_created" } }),
        );
        let items = map_all(
            &mut mapper,
            json!({ "type": "response.completed", "response": { "status": "completed" } }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::End(end)) => {
                assert_eq!(end.captured_response_id.as_deref(), Some("resp_created"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn terminal_id_takes_precedence_over_created() {
        let mut mapper = CodexEventMapper::new();
        let _ = map_all(
            &mut mapper,
            json!({ "type": "response.created", "response": { "id": "resp_created" } }),
        );
        let items = map_all(
            &mut mapper,
            json!({ "type": "response.completed", "response": { "id": "resp_final", "status": "completed" } }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::End(end)) => {
                assert_eq!(end.captured_response_id.as_deref(), Some("resp_final"));
            }
            _ => panic!("expected End"),
        }
    }

    #[test]
    fn second_message_backfilled_only_on_done_is_kept() {
        // M2 (mapper level): index 0 streams a delta then done; index 1 delivers
        // its text only via done. Both must produce text chunks.
        let mut mapper = CodexEventMapper::new();
        let _ = map_all(
            &mut mapper,
            json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "First" }),
        );
        // done for index 0 with matching authoritative text -> no re-emit.
        assert!(
            map_all(
                &mut mapper,
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": { "type": "message", "content": [{ "type": "output_text", "text": "First" }] }
                }),
            )
            .is_empty()
        );
        // done for index 1 with no prior delta -> backfill "Second".
        let items = map_all(
            &mut mapper,
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": { "type": "message", "content": [{ "type": "output_text", "text": "Second" }] }
            }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::Chunk(c)) => assert_eq!(c.content, "Second"),
            _ => panic!("expected backfilled text chunk"),
        }
    }

    #[test]
    fn message_done_appends_authoritative_correction_tail() {
        // A backend correction that extends the streamed prefix emits only the tail.
        let mut mapper = CodexEventMapper::new();
        let _ = map_all(
            &mut mapper,
            json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "Hel" }),
        );
        let items = map_all(
            &mut mapper,
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": { "type": "message", "content": [{ "type": "output_text", "text": "Hello" }] }
            }),
        );
        match &items[0] {
            MappedItem::Stream(ChatStreamEvent::Chunk(c)) => assert_eq!(c.content, "lo"),
            _ => panic!("expected tail chunk"),
        }
    }
}
