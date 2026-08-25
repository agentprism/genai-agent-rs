//! Replay-aware assistant streaming, assembly, and terminal records from
//! Architecture v2 part 1 §3.3 and part 2 §1.3–§1.9, §2.1, and §9.2–§9.3.

use crate::{
    ApiId, AssistantFinish, AssistantFinishReason, AssistantMessage, AssistantMessageDiagnostic,
    ContentBlock, ContentBlockId, Cost, DEFERRED_HANDLE_SCHEMA_VERSION, DeferredHandle,
    LocalBoxStream, MessageId, ModelId, OpaquePayload, ProviderId, PublicError,
    REPLAY_ENVELOPE_SCHEMA_VERSION, ReplayApplicability, ReplayCompleteness, ReplayEnvelope,
    ReplayItem, ReplayItemId, ReplayKind, ReplayScope, ReplayTarget, SendBoxStream, Timestamp,
    ToolCall, ToolCallId, Usage, UsageSource,
};
use futures_core::{Stream, stream::FusedStream};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::{fmt, pin::Pin, task::Context, task::Poll};

const ANTHROPIC_REDACTED_THINKING_KIND: &str = "anthropic.messages.redacted-thinking";
const BEDROCK_REDACTED_REASONING_KIND: &str = "bedrock.converse.redacted-reasoning";
/// pi-messages replay artifact carrying a completed text signature.
pub const PI_MESSAGES_TEXT_SIGNATURE_KIND: &str = "pi.messages.text-signature";
/// pi-messages replay artifact carrying a completed thinking signature.
pub const PI_MESSAGES_THINKING_SIGNATURE_KIND: &str = "pi.messages.thinking-signature";
/// pi-messages marker retaining the server's redacted-thinking flag.
pub const PI_MESSAGES_REDACTED_THINKING_KIND: &str = "pi.messages.redacted-thinking";
/// pi-messages marker retaining an explicitly false redacted-thinking field.
pub const PI_MESSAGES_VISIBLE_THINKING_KIND: &str = "pi.messages.visible-thinking";

/// Parse the useful semantic prefix of streamed tool arguments.
///
/// Pi first tries ordinary JSON with its string-literal repair pass, then its
/// `partial-json` parser, and finally the repaired input with that parser. A
/// malformed fragment that cannot be recovered becomes an empty object. This
/// local port keeps the same observable contract without making parser scratch
/// part of [`AssistantMessage`].
fn parse_streaming_json(input: &str) -> Value {
    if input.trim().is_empty() {
        return Value::Object(Map::new());
    }

    if let Ok(value) = serde_json::from_str(input) {
        return value;
    }

    let repaired = repair_json(input);
    if repaired != input
        && let Ok(value) = serde_json::from_str(&repaired)
    {
        return value;
    }

    PartialJsonParser::new(input)
        .parse()
        .or_else(|()| PartialJsonParser::new(&repaired).parse())
        .unwrap_or_else(|()| Value::Object(Map::new()))
}

/// Repair raw control characters and invalid JSON escapes in string literals,
/// matching pinned Pi's `repairJson` preprocessing.
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

            // Preserve the following character while making the backslash
            // literal. It will be consumed on the next iteration.
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
                use fmt::Write as _;
                write!(repaired, "\\u{:04x}", u32::from(control))
                    .expect("writing to String cannot fail");
            }
            _ => repaired.push(character),
        }
        index += 1;
    }

    repaired
}

/// JSON-prefix parser matching the `partial-json` defaults used by pinned Pi.
struct PartialJsonParser<'a> {
    input: &'a [u8],
    index: usize,
}

impl<'a> PartialJsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.trim().as_bytes(),
            index: 0,
        }
    }

    fn parse(mut self) -> Result<Value, ()> {
        self.parse_any()
    }

    fn parse_any(&mut self) -> Result<Value, ()> {
        self.skip_whitespace();
        let remaining = self.input.get(self.index..).ok_or(())?;
        let first = *remaining.first().ok_or(())?;

        match first {
            b'"' => self.parse_string().map(Value::String),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            _ if literal_is_complete_or_prefix(remaining, b"null") => {
                self.index = (self.index + b"null".len()).min(self.input.len());
                Ok(Value::Null)
            }
            _ if literal_is_complete_or_prefix(remaining, b"true") => {
                self.index = (self.index + b"true".len()).min(self.input.len());
                Ok(Value::Bool(true))
            }
            _ if literal_is_complete_or_prefix(remaining, b"false") => {
                self.index = (self.index + b"false".len()).min(self.input.len());
                Ok(Value::Bool(false))
            }
            // `partial-json` returns JavaScript non-finite numbers for these
            // complete or partial literals. Pi later persists tool arguments
            // with `JSON.stringify`, which retains the member as `null`.
            _ if literal_is_complete_or_prefix(remaining, b"Infinity") => {
                self.index = (self.index + b"Infinity".len()).min(self.input.len());
                Ok(Value::Null)
            }
            _ if remaining.starts_with(b"-Infinity")
                || (remaining.len() > 1
                    && remaining.len() < b"-Infinity".len()
                    && b"-Infinity".starts_with(remaining)) =>
            {
                self.index = (self.index + b"-Infinity".len()).min(self.input.len());
                Ok(Value::Null)
            }
            _ if literal_is_complete_or_prefix(remaining, b"NaN") => {
                self.index = (self.index + b"NaN".len()).min(self.input.len());
                Ok(Value::Null)
            }
            _ => self.parse_number().map(Value::Number),
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        let start = self.index;
        if self.input.get(start) != Some(&b'"') {
            return Err(());
        }
        self.index += 1;
        let mut escaped = false;

        while let Some(&byte) = self.input.get(self.index) {
            if byte == b'"' && !escaped {
                self.index += 1;
                return serde_json::from_slice(&self.input[start..self.index]).map_err(|_| ());
            }
            escaped = if byte == b'\\' { !escaped } else { false };
            self.index += 1;
        }

        let mut end = self.index;
        if escaped {
            end = end.saturating_sub(1);
        }
        let mut candidate = self.input[start..end].to_vec();
        candidate.push(b'"');
        if let Ok(value) = serde_json::from_slice(&candidate) {
            return Ok(value);
        }

        let last_backslash = self.input[start..self.index]
            .iter()
            .rposition(|byte| *byte == b'\\')
            .map(|relative| start + relative)
            .ok_or(())?;
        candidate.clear();
        candidate.extend_from_slice(&self.input[start..last_backslash]);
        candidate.push(b'"');
        serde_json::from_slice(&candidate).map_err(|_| ())
    }

    fn parse_object(&mut self) -> Result<Value, ()> {
        self.index += 1;
        self.skip_whitespace();
        let mut object = Map::new();

        loop {
            self.skip_whitespace();
            match self.input.get(self.index) {
                Some(b'}') => {
                    self.index += 1;
                    return Ok(Value::Object(object));
                }
                None => return Ok(Value::Object(object)),
                Some(_) => {}
            }

            let key = match self.parse_string() {
                Ok(key) => key,
                Err(()) => return Ok(Value::Object(object)),
            };
            self.skip_whitespace();
            // `partial-json` consumes the next byte as the colon and lets the
            // enclosing partial object retain previously completed members if
            // the value is absent or malformed.
            if self.index < self.input.len() {
                self.index += 1;
            }
            let value = match self.parse_any() {
                Ok(value) => value,
                Err(()) => return Ok(Value::Object(object)),
            };
            object.insert(key, value);
            self.skip_whitespace();
            if self.input.get(self.index) == Some(&b',') {
                self.index += 1;
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, ()> {
        self.index += 1;
        let mut array = Vec::new();

        loop {
            self.skip_whitespace();
            match self.input.get(self.index) {
                Some(b']') => {
                    self.index += 1;
                    return Ok(Value::Array(array));
                }
                None => return Ok(Value::Array(array)),
                Some(_) => {}
            }

            let value = match self.parse_any() {
                Ok(value) => value,
                Err(()) => return Ok(Value::Array(array)),
            };
            array.push(value);
            self.skip_whitespace();
            if self.input.get(self.index) == Some(&b',') {
                self.index += 1;
            }
        }
    }

    fn parse_number(&mut self) -> Result<Number, ()> {
        let start = self.index;
        while let Some(byte) = self.input.get(self.index) {
            if matches!(byte, b',' | b']' | b'}') {
                break;
            }
            self.index += 1;
        }
        let candidate = std::str::from_utf8(&self.input[start..self.index])
            .map_err(|_| ())?
            .trim();
        if candidate == "-" || candidate.is_empty() {
            return Err(());
        }
        if let Ok(number) = candidate.parse::<Number>() {
            return Ok(number);
        }

        // `partial-json` 0.1.7 retries malformed numbers only by truncating at
        // the last lowercase `e`. It does not recover a trailing decimal point
        // or an uppercase exponent marker.
        candidate
            .rfind('e')
            .and_then(|exponent| candidate[..exponent].parse::<Number>().ok())
            .ok_or(())
    }

    fn skip_whitespace(&mut self) {
        while self
            .input
            .get(self.index)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.index += 1;
        }
    }
}

fn literal_is_complete_or_prefix(remaining: &[u8], literal: &[u8]) -> bool {
    remaining.starts_with(literal)
        || (remaining.len() < literal.len() && literal.starts_with(remaining))
}

/// The canonical kind of a streamed content block (Architecture v2 part 2
/// §1.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockKind {
    /// Visible UTF-8 text.
    Text,
    /// Visible or provider-redacted reasoning.
    Thinking,
    /// A model-requested tool invocation.
    ToolCall,
}

/// A lossless normalized assistant-stream event (Architecture v2 part 2 §1.3).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantEvent {
    /// Establishes stable message identity before any content event.
    MessageStarted {
        /// Stable message identifier.
        message_id: MessageId,
        /// Provider serving the request.
        provider: ProviderId,
        /// API family serving the request.
        api: ApiId,
        /// Model requested by the caller.
        model: ModelId,
    },

    /// Adds provider response identity discovered during streaming.
    ResponseMetadata {
        /// Provider response identifier, when reported.
        response_id: Option<String>,
        /// Concrete response model, when reported.
        response_model: Option<ModelId>,
        /// Provider indication that the model explicitly ended its turn.
        end_turn: Option<bool>,
    },

    /// Starts a canonical content block.
    ContentBlockStarted {
        /// Stable block identifier.
        block_id: ContentBlockId,
        /// Canonical content order.
        content_index: u32,
        /// Canonical block kind.
        kind: ContentBlockKind,
    },

    /// Appends visible text to a text block.
    TextDelta {
        /// Stable target block.
        block_id: ContentBlockId,
        /// UTF-8 fragment in provider order.
        delta: String,
    },

    /// Replaces visible text with a provider-authoritative completed value.
    TextReplaced {
        /// Stable target block.
        block_id: ContentBlockId,
        /// Complete authoritative visible text.
        text: String,
    },

    /// Appends visible reasoning to a thinking block.
    ThinkingDelta {
        /// Stable target block.
        block_id: ContentBlockId,
        /// UTF-8 fragment in provider order.
        delta: String,
    },

    /// Replaces visible reasoning with a provider-authoritative completed
    /// value.
    ThinkingReplaced {
        /// Stable target block.
        block_id: ContentBlockId,
        /// Complete authoritative visible reasoning.
        thinking: String,
    },

    /// Appends one redacted provider/runtime diagnostic to the partial
    /// assistant record.
    DiagnosticAdded {
        /// Diagnostic to retain through terminal assembly and persistence.
        diagnostic: AssistantMessageDiagnostic,
    },

    /// Establishes or completes tool-call identity and name metadata.
    ToolCallMetadata {
        /// Stable canonical block.
        block_id: ContentBlockId,
        /// Stable tool-call identifier.
        call_id: ToolCallId,
        /// Tool name when available in this provider event.
        name: Option<String>,
    },

    /// Replaces tool-call identity and name with a provider-authoritative
    /// completed value.
    ToolCallMetadataReplaced {
        /// Stable canonical block.
        block_id: ContentBlockId,
        /// Complete authoritative tool-call identifier.
        call_id: ToolCallId,
        /// Complete authoritative tool name.
        name: String,
    },

    /// Appends an exact raw tool-argument JSON fragment.
    ToolArgumentsDelta {
        /// Stable tool-call block.
        block_id: ContentBlockId,
        /// JSON fragment in provider order.
        delta: String,
    },

    /// Replaces the accumulated raw tool-argument JSON with the provider's
    /// authoritative final value. OpenAI Responses may supply non-prefix
    /// arguments on `response.output_item.done`; an append-only event cannot
    /// losslessly represent that mutable Pi behavior.
    ToolArgumentsReplaced {
        /// Stable tool-call block.
        block_id: ContentBlockId,
        /// Complete authoritative raw JSON arguments.
        arguments: String,
    },

    /// Starts one ordered provider replay artifact.
    ReplayItemStarted {
        /// Stable replay-item identifier.
        item_id: ReplayItemId,
        /// Original provider-output ordinal.
        ordinal: u32,
        /// Canonical or provider-output target.
        target: ReplayTarget,
        /// Open API-family replay kind.
        kind: ReplayKind,
        /// Scope in which an encoder may reuse the artifact.
        applicability: ReplayApplicability,
    },

    /// Replaces or appends opaque replay bytes.
    ReplayData {
        /// Stable replay item receiving the operation.
        item_id: ReplayItemId,
        /// Lossless payload mutation.
        operation: ReplayDataOperation,
    },

    /// Discards a replay artifact that the provider superseded before the
    /// content block completed. Bedrock uses this when a reasoning block first
    /// streams a signature and then switches to authoritative redacted bytes.
    ReplayItemDiscarded {
        /// Stable replay item that is no longer part of the provider output.
        item_id: ReplayItemId,
    },

    /// Marks one replay item complete and eligible for applicable replay.
    ReplayItemFinished {
        /// Stable replay-item identifier.
        item_id: ReplayItemId,
    },

    /// Marks one canonical content block complete.
    ContentBlockFinished {
        /// Stable block identifier.
        block_id: ContentBlockId,
    },

    /// Replaces the last-known cumulative response usage.
    UsageUpdated {
        /// Authoritative cumulative usage so far.
        cumulative: Usage,
    },

    /// Terminates the stream with a successful assistant record.
    Finished {
        /// Complete replay-valid assistant message.
        message: AssistantMessage,
    },

    /// Terminates the stream with a committed failed assistant record.
    Failed {
        /// Partial content plus structured failure metadata.
        message: AssistantMessage,
    },

    /// Terminates the stream with a committed cancelled assistant record.
    Cancelled {
        /// Partial content plus structured cancellation metadata.
        message: AssistantMessage,
    },
}

impl AssistantEvent {
    /// Returns whether this event terminates an assistant stream.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    /// Returns the terminal assistant message, when this is a terminal event.
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Finished { message } | Self::Failed { message } | Self::Cancelled { message } => {
                Some(message)
            }
            _ => None,
        }
    }
}

/// An opaque replay-payload mutation (Architecture v2 part 2 §1.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ReplayDataOperation {
    /// Replace the payload with UTF-8 data.
    ReplaceUtf8(String),
    /// Append UTF-8 data, initializing an empty UTF-8 payload if needed.
    AppendUtf8(String),
    /// Replace the payload with opaque bytes.
    ReplaceBytes(Vec<u8>),
    /// Append opaque bytes, initializing an empty byte payload if needed.
    AppendBytes(Vec<u8>),
    /// Replace the payload with compatibility-serializer JSON bytes.
    ReplaceJsonBytes(Vec<u8>),
}

/// A portable display reason supplied when terminal assembly is cancelled
/// (Architecture v2 part 2 §2.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CancellationReason {
    /// Human-readable cancellation text.
    pub message: String,
    /// Last known provider request identifier, when available.
    pub request_id: Option<String>,
}

impl CancellationReason {
    /// Creates a cancellation reason without a provider request identifier.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            request_id: None,
        }
    }

    /// Adds the last known provider request identifier.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

#[derive(Clone, Debug)]
struct AssistantMessageBuilder {
    started: bool,
    id: MessageId,
    provider: ProviderId,
    api: ApiId,
    requested_model: ModelId,
    response_model: Option<ModelId>,
    response_id: Option<String>,
    deferred: Option<DeferredHandle>,
    end_turn: Option<bool>,
    diagnostics: Vec<AssistantMessageDiagnostic>,
    usage: Usage,
    cost: Option<Cost>,
    timestamp: Timestamp,
}

impl AssistantMessageBuilder {
    fn new(timestamp: Timestamp) -> Self {
        Self {
            started: false,
            id: MessageId::default(),
            provider: ProviderId::default(),
            api: ApiId::default(),
            requested_model: ModelId::default(),
            response_model: None,
            response_id: None,
            deferred: None,
            end_turn: None,
            diagnostics: Vec::new(),
            usage: Usage::zero(UsageSource::Unknown),
            cost: None,
            timestamp,
        }
    }

    fn replay_scope(&self) -> ReplayScope {
        ReplayScope {
            provider: self.provider.clone(),
            api: self.api.clone(),
            requested_model: self.requested_model.clone(),
            produced_by_model: self
                .response_model
                .clone()
                .unwrap_or_else(|| self.requested_model.clone()),
            protocol_revision: None,
        }
    }
}

#[derive(Clone, Debug)]
enum BlockBuilder {
    Text {
        content_index: u32,
        text: String,
        finished: bool,
    },
    Thinking {
        content_index: u32,
        text: String,
        replay_item: Option<ReplayItemId>,
        finished: bool,
    },
    ToolCall {
        content_index: u32,
        call_id: Option<ToolCallId>,
        name: Option<String>,
        arguments_scratch: String,
        finalized_arguments: Option<Value>,
        finished: bool,
    },
}

impl BlockBuilder {
    fn new(content_index: u32, kind: ContentBlockKind, replay_item: Option<ReplayItemId>) -> Self {
        match kind {
            ContentBlockKind::Text => Self::Text {
                content_index,
                text: String::new(),
                finished: false,
            },
            ContentBlockKind::Thinking => Self::Thinking {
                content_index,
                text: String::new(),
                replay_item,
                finished: false,
            },
            ContentBlockKind::ToolCall => Self::ToolCall {
                content_index,
                call_id: None,
                name: None,
                arguments_scratch: String::new(),
                finalized_arguments: None,
                finished: false,
            },
        }
    }

    fn content_index(&self) -> u32 {
        match self {
            Self::Text { content_index, .. }
            | Self::Thinking { content_index, .. }
            | Self::ToolCall { content_index, .. } => *content_index,
        }
    }

    fn kind(&self) -> ContentBlockKind {
        match self {
            Self::Text { .. } => ContentBlockKind::Text,
            Self::Thinking { .. } => ContentBlockKind::Thinking,
            Self::ToolCall { .. } => ContentBlockKind::ToolCall,
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Text { finished, .. }
            | Self::Thinking { finished, .. }
            | Self::ToolCall { finished, .. } => *finished,
        }
    }
}

#[derive(Clone, Debug)]
struct ReplayItemBuilder {
    id: ReplayItemId,
    ordinal: u32,
    target: ReplayTarget,
    kind: ReplayKind,
    applicability: ReplayApplicability,
    payload: Option<OpaquePayload>,
    finished: bool,
}

impl ReplayItemBuilder {
    fn apply(&mut self, operation: &ReplayDataOperation) -> Result<(), AssemblyError> {
        if self.finished {
            return Err(AssemblyError::ReplayItemAlreadyFinished(self.id.clone()));
        }

        match operation {
            ReplayDataOperation::ReplaceUtf8(value) => {
                self.payload = Some(OpaquePayload::Utf8(value.clone()));
            }
            ReplayDataOperation::AppendUtf8(fragment) => match &mut self.payload {
                None => self.payload = Some(OpaquePayload::Utf8(fragment.clone())),
                Some(OpaquePayload::Utf8(value)) => value.push_str(fragment),
                Some(OpaquePayload::Bytes(_) | OpaquePayload::JsonBytes(_)) => {
                    return Err(AssemblyError::ReplayPayloadEncodingMismatch(
                        self.id.clone(),
                    ));
                }
            },
            ReplayDataOperation::ReplaceBytes(value) => {
                self.payload = Some(OpaquePayload::Bytes(value.clone()));
            }
            ReplayDataOperation::AppendBytes(fragment) => match &mut self.payload {
                None => self.payload = Some(OpaquePayload::Bytes(fragment.clone())),
                Some(OpaquePayload::Bytes(value)) => value.extend_from_slice(fragment),
                Some(OpaquePayload::Utf8(_) | OpaquePayload::JsonBytes(_)) => {
                    return Err(AssemblyError::ReplayPayloadEncodingMismatch(
                        self.id.clone(),
                    ));
                }
            },
            ReplayDataOperation::ReplaceJsonBytes(value) => {
                self.payload = Some(OpaquePayload::JsonBytes(value.clone()));
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> ReplayItem {
        ReplayItem {
            id: self.id.clone(),
            ordinal: self.ordinal,
            target: self.target.clone(),
            kind: self.kind.clone(),
            applicability: self.applicability,
            completeness: if self.finished {
                ReplayCompleteness::Complete
            } else {
                ReplayCompleteness::Incomplete
            },
            // An item can fail before its first data operation. Incomplete items
            // are never replayed, so an empty byte payload is a lossless neutral
            // representation of "no provider bytes observed".
            payload: self
                .payload
                .clone()
                .unwrap_or_else(|| OpaquePayload::Bytes(Vec::new())),
        }
    }
}

/// The authoritative replay-aware consumer of [`AssistantEvent`] values
/// (Architecture v2 part 2 §1.3).
#[derive(Clone, Debug)]
pub struct AssistantAssembler {
    message: AssistantMessageBuilder,
    blocks: IndexMap<ContentBlockId, BlockBuilder>,
    replay: IndexMap<ReplayItemId, ReplayItemBuilder>,
    terminal: bool,
    terminal_message: Option<AssistantMessage>,
}

impl Default for AssistantAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantAssembler {
    /// Creates an unstarted assembler with a zero timestamp.
    ///
    /// Provider adapters with an injected clock should use
    /// [`Self::with_timestamp`] before applying `MessageStarted`.
    pub fn new() -> Self {
        Self::with_timestamp(Timestamp::default())
    }

    /// Creates an unstarted assembler with an explicit message timestamp.
    pub fn with_timestamp(timestamp: Timestamp) -> Self {
        Self {
            message: AssistantMessageBuilder::new(timestamp),
            blocks: IndexMap::new(),
            replay: IndexMap::new(),
            terminal: false,
            terminal_message: None,
        }
    }

    /// Applies one normalized stream event.
    pub fn apply(&mut self, event: &AssistantEvent) -> Result<(), AssemblyError> {
        if self.terminal {
            return Err(AssemblyError::EventAfterTerminal);
        }

        match event {
            AssistantEvent::MessageStarted {
                message_id,
                provider,
                api,
                model,
            } => {
                if self.message.started {
                    return Err(AssemblyError::MessageAlreadyStarted);
                }
                self.message.started = true;
                self.message.id = message_id.clone();
                self.message.provider = provider.clone();
                self.message.api = api.clone();
                self.message.requested_model = model.clone();
            }
            AssistantEvent::ResponseMetadata {
                response_id,
                response_model,
                end_turn,
            } => {
                self.require_started()?;
                if let Some(response_id) = response_id {
                    match &self.message.response_id {
                        None => self.message.response_id = Some(response_id.clone()),
                        Some(existing) if existing == response_id => {}
                        Some(_) => return Err(AssemblyError::ResponseIdChanged),
                    }
                }
                if let Some(response_model) = response_model {
                    match &self.message.response_model {
                        None => self.message.response_model = Some(response_model.clone()),
                        Some(existing) if existing == response_model => {}
                        Some(_) => return Err(AssemblyError::ResponseModelChanged),
                    }
                }
                if let Some(end_turn) = end_turn {
                    match self.message.end_turn {
                        None => self.message.end_turn = Some(*end_turn),
                        Some(existing) if existing == *end_turn => {}
                        Some(_) => return Err(AssemblyError::EndTurnChanged),
                    }
                }
            }
            AssistantEvent::ContentBlockStarted {
                block_id,
                content_index,
                kind,
            } => {
                self.require_started()?;
                self.start_block(block_id.clone(), *content_index, *kind)?;
            }
            AssistantEvent::TextDelta { block_id, delta } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::Text { text, finished, .. } if !*finished => {
                        text.push_str(delta);
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::Text,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::TextReplaced { block_id, text } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::Text {
                        text: current,
                        finished,
                        ..
                    } if !*finished => current.clone_from(text),
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::Text,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ThinkingDelta { block_id, delta } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::Thinking { text, finished, .. } if !*finished => {
                        text.push_str(delta);
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::Thinking,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ThinkingReplaced { block_id, thinking } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::Thinking { text, finished, .. } if !*finished => {
                        text.clone_from(thinking)
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::Thinking,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::DiagnosticAdded { diagnostic } => {
                self.require_started()?;
                self.message.diagnostics.push(diagnostic.clone());
            }
            AssistantEvent::ToolCallMetadata {
                block_id,
                call_id,
                name,
            } => {
                self.require_started()?;
                if !self.blocks.contains_key(block_id) {
                    // The worked OpenAI Responses and Google sequences use
                    // ToolCallMetadata as the tool block's first semantic event.
                    let content_index = u32::try_from(self.blocks.len())
                        .map_err(|_| AssemblyError::TooManyContentBlocks)?;
                    self.start_block(block_id.clone(), content_index, ContentBlockKind::ToolCall)?;
                }
                match self.block_mut(block_id)? {
                    BlockBuilder::ToolCall {
                        call_id: existing_call_id,
                        name: existing_name,
                        finished,
                        ..
                    } if !*finished => {
                        if let Some(existing) = existing_call_id {
                            if existing != call_id {
                                return Err(AssemblyError::ToolCallIdentityChanged(
                                    block_id.clone(),
                                ));
                            }
                        } else {
                            *existing_call_id = Some(call_id.clone());
                        }

                        if let Some(name) = name {
                            if let Some(existing) = existing_name {
                                if existing != name {
                                    return Err(AssemblyError::ToolCallIdentityChanged(
                                        block_id.clone(),
                                    ));
                                }
                            } else {
                                *existing_name = Some(name.clone());
                            }
                        }
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::ToolCall,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ToolCallMetadataReplaced {
                block_id,
                call_id,
                name,
            } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::ToolCall {
                        call_id: existing_call_id,
                        name: existing_name,
                        finished,
                        ..
                    } if !*finished => {
                        *existing_call_id = Some(call_id.clone());
                        *existing_name = Some(name.clone());
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::ToolCall,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ToolArgumentsDelta { block_id, delta } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::ToolCall {
                        arguments_scratch,
                        finalized_arguments,
                        finished,
                        ..
                    } if !*finished => {
                        arguments_scratch.push_str(delta);
                        *finalized_arguments = None;
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::ToolCall,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ToolArgumentsReplaced {
                block_id,
                arguments,
            } => {
                self.require_started()?;
                match self.block_mut(block_id)? {
                    BlockBuilder::ToolCall {
                        arguments_scratch,
                        finalized_arguments,
                        finished,
                        ..
                    } if !*finished => {
                        arguments_scratch.clone_from(arguments);
                        *finalized_arguments = None;
                    }
                    block if block.is_finished() => {
                        return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                    }
                    block => {
                        return Err(AssemblyError::WrongContentBlockKind {
                            block_id: block_id.clone(),
                            expected: ContentBlockKind::ToolCall,
                            actual: block.kind(),
                        });
                    }
                }
            }
            AssistantEvent::ReplayItemStarted {
                item_id,
                ordinal,
                target,
                kind,
                applicability,
            } => {
                self.require_started()?;
                if self.replay.contains_key(item_id) {
                    return Err(AssemblyError::DuplicateReplayItem(item_id.clone()));
                }
                if self.replay.values().any(|item| item.ordinal == *ordinal) {
                    return Err(AssemblyError::DuplicateReplayOrdinal(*ordinal));
                }
                self.replay.insert(
                    item_id.clone(),
                    ReplayItemBuilder {
                        id: item_id.clone(),
                        ordinal: *ordinal,
                        target: target.clone(),
                        kind: kind.clone(),
                        applicability: *applicability,
                        payload: None,
                        finished: false,
                    },
                );
            }
            AssistantEvent::ReplayData { item_id, operation } => {
                self.require_started()?;
                self.replay
                    .get_mut(item_id)
                    .ok_or_else(|| AssemblyError::UnknownReplayItem(item_id.clone()))?
                    .apply(operation)?;
            }
            AssistantEvent::ReplayItemDiscarded { item_id } => {
                self.require_started()?;
                self.replay
                    .shift_remove(item_id)
                    .ok_or_else(|| AssemblyError::UnknownReplayItem(item_id.clone()))?;
                for block in self.blocks.values_mut() {
                    if let BlockBuilder::Thinking { replay_item, .. } = block
                        && replay_item.as_ref() == Some(item_id)
                    {
                        *replay_item = None;
                    }
                }
            }
            AssistantEvent::ReplayItemFinished { item_id } => {
                self.require_started()?;
                let item = self
                    .replay
                    .get_mut(item_id)
                    .ok_or_else(|| AssemblyError::UnknownReplayItem(item_id.clone()))?;
                if item.finished {
                    return Err(AssemblyError::ReplayItemAlreadyFinished(item_id.clone()));
                }
                if item.payload.is_none() {
                    return Err(AssemblyError::ReplayItemMissingPayload(item_id.clone()));
                }
                item.finished = true;
            }
            AssistantEvent::ContentBlockFinished { block_id } => {
                self.require_started()?;
                let block = self.block_mut(block_id)?;
                if block.is_finished() {
                    return Err(AssemblyError::ContentBlockAlreadyFinished(block_id.clone()));
                }
                if let BlockBuilder::ToolCall {
                    arguments_scratch,
                    finalized_arguments,
                    ..
                } = block
                {
                    let json = if arguments_scratch.trim().is_empty() {
                        "{}"
                    } else {
                        arguments_scratch.as_str()
                    };
                    *finalized_arguments = Some(parse_streaming_json(json));
                }
                match block {
                    BlockBuilder::Text { finished, .. }
                    | BlockBuilder::Thinking { finished, .. }
                    | BlockBuilder::ToolCall { finished, .. } => *finished = true,
                }
            }
            AssistantEvent::UsageUpdated { cumulative } => {
                self.require_started()?;
                // Pi overwrites its mutable partial usage with the newest
                // authoritative provider values. Rust events label the same
                // value explicitly as cumulative, so never add it as a delta.
                self.message.usage = cumulative.clone();
            }
            AssistantEvent::Finished { message } => {
                self.apply_terminal_message(message, AssistantFinishReason::Stop, false)?;
            }
            AssistantEvent::Failed { message } => {
                self.apply_terminal_message(message, AssistantFinishReason::Error, true)?;
            }
            AssistantEvent::Cancelled { message } => {
                self.apply_terminal_message(message, AssistantFinishReason::Aborted, true)?;
            }
        }
        Ok(())
    }

    /// Returns an immutable view of stable identity plus owned canonical and
    /// replay snapshots. No parser scratch is exposed.
    pub fn snapshot(&self) -> AssistantMessageSnapshot {
        AssistantMessageSnapshot {
            id: self.message.id.clone(),
            provider: self.message.provider.clone(),
            api: self.message.api.clone(),
            requested_model: self.message.requested_model.clone(),
            response_model: self.message.response_model.clone(),
            response_id: self.message.response_id.clone(),
            deferred: self.message.deferred.clone(),
            end_turn: self.message.end_turn,
            diagnostics: self.message.diagnostics.clone(),
            content: self.content_snapshot(true),
            replay: self.replay_snapshot(),
            usage: self.message.usage.clone(),
            cost: self.message.cost.clone(),
            timestamp: self.message.timestamp,
            terminal_message: self.terminal_message.clone(),
        }
    }

    /// Finishes a successful message after validating every content and replay
    /// item as complete (Architecture v2 part 2 §1.3 and R2).
    pub fn finish_completed(
        self,
        finish: AssistantFinish,
    ) -> Result<AssistantMessage, AssemblyError> {
        self.require_started()?;
        Self::validate_successful_finish(&finish)?;
        self.validate_successful_blocks()?;
        self.validate_successful_replay()?;
        self.build_message(finish, false)
    }

    /// Finishes a successful deferred response with its durable provider
    /// handle.
    pub fn finish_deferred(
        mut self,
        handle: DeferredHandle,
    ) -> Result<AssistantMessage, AssemblyError> {
        self.message.deferred = Some(handle);
        self.finish_completed(AssistantFinish {
            reason: AssistantFinishReason::Deferred,
            raw_provider_reason: None,
            error: None,
        })
    }

    /// Finishes a failed message, retaining complete replay items and marking
    /// unfinished items incomplete (Architecture v2 part 2 §2.1).
    pub fn finish_failed(
        self,
        error: PublicError,
        raw_provider_reason: Option<String>,
    ) -> AssistantMessage {
        self.build_terminal_without_validation(AssistantFinish {
            reason: AssistantFinishReason::Error,
            raw_provider_reason,
            error: Some(error.sanitized(&[])),
        })
    }

    /// Finishes a cancelled message with normalized code `cancelled`, partial
    /// content, and last-known cumulative metadata (Architecture v2 part 2
    /// §2.1).
    pub fn finish_cancelled(self, reason: CancellationReason) -> AssistantMessage {
        self.build_terminal_without_validation(AssistantFinish {
            reason: AssistantFinishReason::Aborted,
            raw_provider_reason: None,
            error: Some(PublicError {
                code: "cancelled".into(),
                message: reason.message,
                retryable: false,
                provider_code: None,
                status: None,
                request_id: reason.request_id,
            }),
        })
    }

    fn require_started(&self) -> Result<(), AssemblyError> {
        if self.message.started {
            Ok(())
        } else {
            Err(AssemblyError::MessageNotStarted)
        }
    }

    fn start_block(
        &mut self,
        block_id: ContentBlockId,
        content_index: u32,
        kind: ContentBlockKind,
    ) -> Result<(), AssemblyError> {
        if self.blocks.contains_key(&block_id) {
            return Err(AssemblyError::DuplicateContentBlock(block_id));
        }
        let expected =
            u32::try_from(self.blocks.len()).map_err(|_| AssemblyError::TooManyContentBlocks)?;
        if content_index != expected {
            return Err(AssemblyError::NonSequentialContentIndex {
                expected,
                actual: content_index,
            });
        }
        // OpenAI Responses replay items retain provider-global ordering with a
        // ProviderOutputItem target. Its normalized event sequence brackets
        // the corresponding canonical block start inside the replay item's
        // lifetime (part 2 §1.6). Preserve that otherwise-unrepresentable
        // association as the thinking block's persisted reverse link.
        let replay_item = if kind == ContentBlockKind::Thinking {
            self.replay
                .values()
                .rev()
                .find(|candidate| {
                    !candidate.finished
                        && matches!(candidate.target, ReplayTarget::ProviderOutputItem { .. })
                        && !self.blocks.values().any(|block| {
                            matches!(
                                block,
                                BlockBuilder::Thinking {
                                    replay_item: Some(existing),
                                    ..
                                } if existing == &candidate.id
                            )
                        })
                })
                .map(|candidate| candidate.id.clone())
        } else {
            None
        };
        self.blocks.insert(
            block_id,
            BlockBuilder::new(content_index, kind, replay_item),
        );
        Ok(())
    }

    fn block_mut(&mut self, block_id: &ContentBlockId) -> Result<&mut BlockBuilder, AssemblyError> {
        self.blocks
            .get_mut(block_id)
            .ok_or_else(|| AssemblyError::UnknownContentBlock(block_id.clone()))
    }

    fn replay_snapshot(&self) -> ReplayEnvelope {
        let mut items = self
            .replay
            .values()
            .map(ReplayItemBuilder::snapshot)
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item.ordinal);
        ReplayEnvelope {
            schema_version: REPLAY_ENVELOPE_SCHEMA_VERSION,
            source: self.message.replay_scope(),
            items,
        }
    }

    fn content_snapshot(&self, allow_partial: bool) -> Vec<ContentBlock> {
        let mut blocks = self.blocks.iter().collect::<Vec<_>>();
        blocks.sort_by_key(|(_, block)| block.content_index());
        blocks
            .into_iter()
            .filter_map(|(block_id, block)| {
                self.build_content_block(block_id, block, allow_partial)
            })
            .collect()
    }

    fn build_content_block(
        &self,
        block_id: &ContentBlockId,
        block: &BlockBuilder,
        allow_partial: bool,
    ) -> Option<ContentBlock> {
        match block {
            BlockBuilder::Text { text, .. } => Some(ContentBlock::Text {
                id: block_id.clone(),
                text: text.clone(),
            }),
            BlockBuilder::Thinking {
                text,
                replay_item: associated_replay_item,
                ..
            } => {
                let replay_item = self
                    .replay
                    .values()
                    .filter(|item| item.target == ReplayTarget::ContentBlock(block_id.clone()))
                    .min_by_key(|item| item.ordinal)
                    .or_else(|| {
                        associated_replay_item
                            .as_ref()
                            .and_then(|item_id| self.replay.get(item_id))
                    });
                let redacted = replay_item.is_some_and(|item| {
                    matches!(
                        item.kind.as_str(),
                        ANTHROPIC_REDACTED_THINKING_KIND
                            | BEDROCK_REDACTED_REASONING_KIND
                            | PI_MESSAGES_REDACTED_THINKING_KIND
                    )
                });
                Some(ContentBlock::Thinking {
                    id: block_id.clone(),
                    text: text.clone(),
                    redacted,
                    replay_item: replay_item.map(|item| item.id.clone()),
                })
            }
            BlockBuilder::ToolCall {
                call_id,
                name,
                arguments_scratch,
                finalized_arguments,
                ..
            } => {
                let arguments = finalized_arguments.clone().or_else(|| {
                    if !allow_partial {
                        return None;
                    }
                    let json = if arguments_scratch.trim().is_empty() {
                        "{}"
                    } else {
                        arguments_scratch.as_str()
                    };
                    Some(parse_streaming_json(json))
                })?;
                let call_id = if allow_partial {
                    call_id.clone().unwrap_or_default()
                } else {
                    call_id.clone()?
                };
                let name = if allow_partial {
                    name.clone().unwrap_or_default()
                } else {
                    name.clone()?
                };
                Some(ContentBlock::ToolCall {
                    id: block_id.clone(),
                    call: ToolCall {
                        id: call_id,
                        name,
                        arguments,
                    },
                })
            }
        }
    }

    fn validate_successful_blocks(&self) -> Result<(), AssemblyError> {
        for (block_id, block) in &self.blocks {
            if !block.is_finished() {
                return Err(AssemblyError::IncompleteContentBlock(block_id.clone()));
            }
            if let BlockBuilder::ToolCall {
                call_id,
                name,
                finalized_arguments,
                ..
            } = block
            {
                if call_id.is_none() || name.is_none() {
                    return Err(AssemblyError::MissingToolCallMetadata(block_id.clone()));
                }
                if finalized_arguments.is_none() {
                    return Err(AssemblyError::InvalidToolArguments {
                        block_id: block_id.clone(),
                        message: "tool arguments were not finalized".into(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_successful_replay(&self) -> Result<(), AssemblyError> {
        for item in self.replay.values() {
            if !item.finished {
                return Err(AssemblyError::IncompleteReplayItem(item.id.clone()));
            }
            if item.payload.is_none() {
                return Err(AssemblyError::ReplayItemMissingPayload(item.id.clone()));
            }
            match &item.target {
                ReplayTarget::Message | ReplayTarget::ProviderOutputItem { .. } => {}
                ReplayTarget::ContentBlock(block_id) => {
                    if !self.blocks.contains_key(block_id) {
                        return Err(AssemblyError::ReplayTargetNotFound(item.id.clone()));
                    }
                }
                ReplayTarget::ToolCall(call_id) => {
                    let found = self.blocks.values().any(|block| {
                        matches!(
                            block,
                            BlockBuilder::ToolCall {
                                call_id: Some(existing),
                                ..
                            } if existing == call_id
                        )
                    });
                    if !found {
                        return Err(AssemblyError::ReplayTargetNotFound(item.id.clone()));
                    }
                }
            }
        }
        Ok(())
    }

    fn build_message(
        &self,
        finish: AssistantFinish,
        allow_partial: bool,
    ) -> Result<AssistantMessage, AssemblyError> {
        self.require_started()?;
        self.validate_deferred_handle(&finish)?;
        Ok(AssistantMessage {
            id: self.message.id.clone(),
            provider: self.message.provider.clone(),
            api: self.message.api.clone(),
            requested_model: self.message.requested_model.clone(),
            response_model: self.message.response_model.clone(),
            response_id: self.message.response_id.clone(),
            deferred: self.message.deferred.clone(),
            end_turn: self.message.end_turn,
            diagnostics: self.message.diagnostics.clone(),
            content: self.content_snapshot(allow_partial),
            replay: self.replay_snapshot(),
            usage: self.message.usage.clone(),
            cost: self.message.cost.clone(),
            finish,
            timestamp: self.message.timestamp,
        })
    }

    fn build_terminal_without_validation(self, finish: AssistantFinish) -> AssistantMessage {
        // `finish_failed` and `finish_cancelled` have infallible architecture
        // signatures. Calling them before MessageStarted is a protocol bug; the
        // default identity remains structurally serializable so failure handling
        // itself never panics or loses the terminal record.
        AssistantMessage {
            id: self.message.id.clone(),
            provider: self.message.provider.clone(),
            api: self.message.api.clone(),
            requested_model: self.message.requested_model.clone(),
            response_model: self.message.response_model.clone(),
            response_id: self.message.response_id.clone(),
            deferred: self.message.deferred.clone(),
            end_turn: self.message.end_turn,
            diagnostics: self.message.diagnostics.clone(),
            content: self.content_snapshot(true),
            replay: self.replay_snapshot(),
            usage: self.message.usage.clone(),
            cost: self.message.cost.clone(),
            finish,
            timestamp: self.message.timestamp,
        }
    }

    fn apply_terminal_message(
        &mut self,
        message: &AssistantMessage,
        expected_reason: AssistantFinishReason,
        allow_partial: bool,
    ) -> Result<(), AssemblyError> {
        self.require_started()?;
        match expected_reason {
            AssistantFinishReason::Stop => Self::validate_successful_finish(&message.finish)?,
            AssistantFinishReason::Error => Self::validate_failed_finish(&message.finish)?,
            AssistantFinishReason::Aborted => Self::validate_cancelled_finish(&message.finish)?,
            _ => unreachable!("terminal event variants use only class-marker reasons"),
        }
        if !allow_partial {
            self.validate_successful_blocks()?;
            self.validate_successful_replay()?;
        }

        // MessageStarted does not carry a clock value. A terminal event does,
        // so a consumer applying a complete stream learns the timestamp here.
        self.message.timestamp = message.timestamp;
        // API-family cost calculation is authoritative at the terminal
        // response boundary and deliberately remains separate from Usage.
        self.message.cost = message.cost.clone();
        // Pinned Pi attaches the handle only to the successful terminal
        // message, so it becomes authoritative at this boundary like cost and
        // timestamp rather than through a separate delta event.
        self.message.deferred = message.deferred.clone();
        let assembled = self.build_message(message.finish.clone(), allow_partial)?;
        if assembled != *message {
            return Err(AssemblyError::TerminalMessageMismatch);
        }
        self.terminal = true;
        self.terminal_message = Some(message.clone());
        Ok(())
    }

    fn validate_successful_finish(finish: &AssistantFinish) -> Result<(), AssemblyError> {
        if !matches!(
            finish.reason,
            AssistantFinishReason::Stop
                | AssistantFinishReason::Length
                | AssistantFinishReason::ToolUse
                | AssistantFinishReason::Deferred
        ) || finish.error.is_some()
        {
            return Err(AssemblyError::InvalidSuccessfulFinish);
        }
        Ok(())
    }

    fn validate_deferred_handle(&self, finish: &AssistantFinish) -> Result<(), AssemblyError> {
        match (finish.reason, self.message.deferred.as_ref()) {
            (AssistantFinishReason::Deferred, None) => Err(AssemblyError::MissingDeferredHandle),
            (AssistantFinishReason::Deferred, Some(handle)) => {
                if handle.schema_version != DEFERRED_HANDLE_SCHEMA_VERSION {
                    return Err(AssemblyError::UnsupportedDeferredHandleSchema(
                        handle.schema_version,
                    ));
                }
                Ok(())
            }
            (_, _) => Ok(()),
        }
    }

    fn validate_failed_finish(finish: &AssistantFinish) -> Result<(), AssemblyError> {
        if finish.reason != AssistantFinishReason::Error {
            return Err(AssemblyError::UnexpectedTerminalReason {
                expected: AssistantFinishReason::Error,
                actual: finish.reason,
            });
        }
        if finish.error.is_none() {
            return Err(AssemblyError::MissingTerminalError);
        }
        Ok(())
    }

    fn validate_cancelled_finish(finish: &AssistantFinish) -> Result<(), AssemblyError> {
        if finish.reason != AssistantFinishReason::Aborted {
            return Err(AssemblyError::UnexpectedTerminalReason {
                expected: AssistantFinishReason::Aborted,
                actual: finish.reason,
            });
        }
        let Some(error) = finish.error.as_ref() else {
            return Err(AssemblyError::InvalidCancellationError);
        };
        if finish.raw_provider_reason.is_some()
            || error.code != "cancelled"
            || error.retryable
            || error.provider_code.is_some()
            || error.status.is_some()
        {
            return Err(AssemblyError::InvalidCancellationError);
        }
        Ok(())
    }
}

/// A scratch-free immutable assistant assembly view (Architecture v2 part 2
/// §1.3 and §8.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageSnapshot {
    /// Stable message identifier, empty only before `MessageStarted`.
    pub id: MessageId,
    /// Serving provider, empty only before `MessageStarted`.
    pub provider: ProviderId,
    /// Serving API family, empty only before `MessageStarted`.
    pub api: ApiId,
    /// Requested model, empty only before `MessageStarted`.
    pub requested_model: ModelId,
    /// Last concrete response model.
    pub response_model: Option<ModelId>,
    /// Last provider response identifier.
    pub response_id: Option<String>,
    /// Durable deferred handle after a deferred terminal event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    /// Last provider end-turn indication.
    pub end_turn: Option<bool>,
    /// Redacted diagnostics observed so far.
    pub diagnostics: Vec<AssistantMessageDiagnostic>,
    /// Canonical partial content without JSON or binary parser scratch.
    pub content: Vec<ContentBlock>,
    /// Ordered replay artifacts, with unfinished items marked incomplete.
    pub replay: ReplayEnvelope,
    /// Last authoritative cumulative usage.
    pub usage: Usage,
    /// Provider-priced response cost once the terminal record is known.
    pub cost: Option<Cost>,
    /// Stable message timestamp.
    pub timestamp: Timestamp,
    /// Terminal record after applying a terminal event.
    pub terminal_message: Option<AssistantMessage>,
}

/// Protocol or assembly failure from [`AssistantAssembler`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssemblyError {
    /// A second message-start event was observed.
    MessageAlreadyStarted,
    /// A content, replay, metadata, usage, or terminal event preceded start.
    MessageNotStarted,
    /// Provider response identity changed after becoming known.
    ResponseIdChanged,
    /// Concrete response model changed after becoming known.
    ResponseModelChanged,
    /// Provider end-turn metadata changed after becoming known.
    EndTurnChanged,
    /// An event was observed after a terminal event.
    EventAfterTerminal,
    /// A stable content block identifier was started twice.
    DuplicateContentBlock(ContentBlockId),
    /// Canonical content indexes did not begin at zero and advance by one.
    NonSequentialContentIndex {
        /// Required next index.
        expected: u32,
        /// Observed index.
        actual: u32,
    },
    /// More content blocks were observed than `u32` can index.
    TooManyContentBlocks,
    /// A content delta or finish referenced an unknown block.
    UnknownContentBlock(ContentBlockId),
    /// An event targeted a block with a different canonical kind.
    WrongContentBlockKind {
        /// Stable block identifier.
        block_id: ContentBlockId,
        /// Kind required by the event.
        expected: ContentBlockKind,
        /// Actual started kind.
        actual: ContentBlockKind,
    },
    /// A content event followed the block's finish.
    ContentBlockAlreadyFinished(ContentBlockId),
    /// Tool call ID or name changed after becoming known.
    ToolCallIdentityChanged(ContentBlockId),
    /// A successful tool block lacked a stable call ID or name.
    MissingToolCallMetadata(ContentBlockId),
    /// Tool arguments were not complete valid JSON.
    InvalidToolArguments {
        /// Stable tool-call block.
        block_id: ContentBlockId,
        /// Sanitized parser diagnostic.
        message: String,
    },
    /// A successful message retained an unfinished content block.
    IncompleteContentBlock(ContentBlockId),
    /// A stable replay item identifier was started twice.
    DuplicateReplayItem(ReplayItemId),
    /// Two replay items claimed the same provider ordinal.
    DuplicateReplayOrdinal(u32),
    /// Replay data or finish referenced an unknown item.
    UnknownReplayItem(ReplayItemId),
    /// Replay data or a second finish followed item completion.
    ReplayItemAlreadyFinished(ReplayItemId),
    /// Append operation encoding did not match the existing replay payload.
    ReplayPayloadEncodingMismatch(ReplayItemId),
    /// A completed replay item received no payload operation.
    ReplayItemMissingPayload(ReplayItemId),
    /// A successful terminal message retained an unfinished replay item.
    IncompleteReplayItem(ReplayItemId),
    /// A complete replay item targeted no assembled block or tool call.
    ReplayTargetNotFound(ReplayItemId),
    /// Error or aborted metadata was supplied to successful completion.
    InvalidSuccessfulFinish,
    /// A deferred successful terminal omitted its durable handle.
    MissingDeferredHandle,
    /// A deferred handle used an unsupported persisted schema.
    UnsupportedDeferredHandleSchema(u32),
    /// A terminal event carried a finish reason inconsistent with its variant.
    UnexpectedTerminalReason {
        /// Required reason, or `Stop` as the successful-reason class marker.
        expected: AssistantFinishReason,
        /// Observed reason.
        actual: AssistantFinishReason,
    },
    /// A failed terminal event omitted its structured public error.
    MissingTerminalError,
    /// A cancelled terminal event did not carry normalized code `cancelled`.
    InvalidCancellationError,
    /// A terminal event's message differed from the authoritative assembly.
    TerminalMessageMismatch,
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessageAlreadyStarted => formatter.write_str("assistant message already started"),
            Self::MessageNotStarted => formatter.write_str("assistant message has not started"),
            Self::ResponseIdChanged => {
                formatter.write_str("provider response identifier changed during streaming")
            }
            Self::ResponseModelChanged => {
                formatter.write_str("concrete response model changed during streaming")
            }
            Self::EndTurnChanged => {
                formatter.write_str("provider end-turn metadata changed during streaming")
            }
            Self::EventAfterTerminal => {
                formatter.write_str("assistant event observed after terminal")
            }
            Self::DuplicateContentBlock(id) => {
                write!(formatter, "content block {id} started twice")
            }
            Self::NonSequentialContentIndex { expected, actual } => write!(
                formatter,
                "content index {actual} is invalid; expected {expected}"
            ),
            Self::TooManyContentBlocks => formatter.write_str("too many assistant content blocks"),
            Self::UnknownContentBlock(id) => write!(formatter, "unknown content block {id}"),
            Self::WrongContentBlockKind {
                block_id,
                expected,
                actual,
            } => write!(
                formatter,
                "content block {block_id} has kind {actual:?}; expected {expected:?}"
            ),
            Self::ContentBlockAlreadyFinished(id) => {
                write!(formatter, "content block {id} is already finished")
            }
            Self::ToolCallIdentityChanged(id) => {
                write!(formatter, "tool-call identity changed for block {id}")
            }
            Self::MissingToolCallMetadata(id) => {
                write!(formatter, "tool-call metadata is incomplete for block {id}")
            }
            Self::InvalidToolArguments { block_id, message } => {
                write!(
                    formatter,
                    "invalid tool arguments for block {block_id}: {message}"
                )
            }
            Self::IncompleteContentBlock(id) => {
                write!(formatter, "content block {id} is incomplete")
            }
            Self::DuplicateReplayItem(id) => write!(formatter, "replay item {id} started twice"),
            Self::DuplicateReplayOrdinal(ordinal) => {
                write!(formatter, "replay ordinal {ordinal} was used twice")
            }
            Self::UnknownReplayItem(id) => write!(formatter, "unknown replay item {id}"),
            Self::ReplayItemAlreadyFinished(id) => {
                write!(formatter, "replay item {id} is already finished")
            }
            Self::ReplayPayloadEncodingMismatch(id) => {
                write!(formatter, "replay payload encoding changed for item {id}")
            }
            Self::ReplayItemMissingPayload(id) => {
                write!(formatter, "replay item {id} has no payload")
            }
            Self::IncompleteReplayItem(id) => write!(formatter, "replay item {id} is incomplete"),
            Self::ReplayTargetNotFound(id) => {
                write!(formatter, "replay target for item {id} was not assembled")
            }
            Self::InvalidSuccessfulFinish => {
                formatter.write_str("successful finish contains failure metadata")
            }
            Self::MissingDeferredHandle => {
                formatter.write_str("deferred assistant finish omitted its durable handle")
            }
            Self::UnsupportedDeferredHandleSchema(version) => {
                write!(
                    formatter,
                    "unsupported deferred handle schema version {version}"
                )
            }
            Self::UnexpectedTerminalReason { expected, actual } => write!(
                formatter,
                "terminal finish reason {actual:?} does not match {expected:?}"
            ),
            Self::MissingTerminalError => {
                formatter.write_str("failed terminal has no public error")
            }
            Self::InvalidCancellationError => {
                formatter.write_str("cancelled terminal must use public error code cancelled")
            }
            Self::TerminalMessageMismatch => {
                formatter.write_str("terminal message differs from assembled stream state")
            }
        }
    }
}

impl std::error::Error for AssemblyError {}

/// A `Send + 'static` assistant stream fused after its first terminal event
/// (Architecture v2 part 2 §9.2–§9.3).
pub struct AssistantStream {
    inner: SendBoxStream<'static, AssistantEvent>,
    done: bool,
}

impl AssistantStream {
    /// Boxes a `Send + 'static` normalized event stream.
    pub fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = AssistantEvent> + Send + 'static,
    {
        Self::from_boxed(Box::pin(inner))
    }

    /// Wraps an already boxed `Send + 'static` event stream.
    pub fn from_boxed(inner: SendBoxStream<'static, AssistantEvent>) -> Self {
        Self { inner, done: false }
    }

    /// Returns whether the stream is fused after a terminal event or EOF.
    pub fn is_terminated(&self) -> bool {
        self.done
    }
}

impl fmt::Debug for AssistantStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssistantStream")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Stream for AssistantStream {
    type Item = AssistantEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                if event.is_terminal() {
                    self.done = true;
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedStream for AssistantStream {
    fn is_terminated(&self) -> bool {
        self.done
    }
}

/// A local-executor assistant stream fused after its first terminal event
/// (Architecture v2 part 2 §9.2).
pub struct LocalAssistantStream {
    inner: LocalBoxStream<'static, AssistantEvent>,
    done: bool,
}

impl LocalAssistantStream {
    /// Boxes a local `'static` normalized event stream.
    pub fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = AssistantEvent> + 'static,
    {
        Self::from_boxed(Box::pin(inner))
    }

    /// Wraps an already boxed local `'static` event stream.
    pub fn from_boxed(inner: LocalBoxStream<'static, AssistantEvent>) -> Self {
        Self { inner, done: false }
    }

    /// Returns whether the stream is fused after a terminal event or EOF.
    pub fn is_terminated(&self) -> bool {
        self.done
    }
}

impl fmt::Debug for LocalAssistantStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalAssistantStream")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Stream for LocalAssistantStream {
    type Item = AssistantEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                if event.is_terminal() {
                    self.done = true;
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedStream for LocalAssistantStream {
    fn is_terminated(&self) -> bool {
        self.done
    }
}
