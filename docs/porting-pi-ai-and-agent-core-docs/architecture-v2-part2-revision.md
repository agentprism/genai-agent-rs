# Architecture v2 — Part 2: source-pinned revision

> **Status: ADOPTED (owner decision, 2026-08-22).** This is the verbatim second answer from the design agent, responding to ten follow-up questions against pi source pinned at `c49906ec77788625aacbdc53ebca6fbe65bd20f5`. It revises Part 1 (`architecture-v2-part1-proposal.md`) and **takes precedence where the two differ**. Its §10 is the project's operational definition of parity; its §10.11 allowlist is the only sanctioned set of divergences from pi; its "Commitment gates" are the acceptance criteria. `goal.md` binds all of this.

# Source baseline and revised architecture

This review is pinned to `earendil-works/pi` `main` at commit `c49906ec77788625aacbdc53ebca6fbe65bd20f5`, committed August 21, 2026.

The deeper source review changes several parts of the earlier proposal:

1. **Opaque provider replay data must be a first-class streaming and persistence protocol.** A final `ProviderOpaque` field alone is insufficient.
2. **Failed and cancelled assistant messages must be committed in-band.** That is not merely a compatibility helper.
3. **Simple-option lowering belongs to each API-family implementation**, although `Models` orchestrates lookup, authentication, effective endpoint resolution, and dispatch.
4. **Context handoff must produce a structured loss report.** Current Pi silently performs several lossy transformations.
5. **The harness should be several crates around `pi-agent-core`, not merely another module in it.**
6. **Pi v4 JSONL should be readable and optionally writable, but it should not constrain the native Rust session format.**
7. **The core can remain executor-neutral**, with Tokio confined to native adapters and an actor facade.

The resulting package structure is:

```text
pi-ai
├── canonical messages and model descriptors
├── replay envelope and stream assembly
├── API-family lowering and wire encoders
├── Models registry, auth, catalogs, retry configuration
└── provider registrations

pi-agent-core
├── agent state machine
├── tools and tool scheduling
├── context projection policies
├── queue polling and lifecycle events
└── no provider catalog, credential store, filesystem, or process runtime

pi-agent-session
├── immutable session entry tree
├── lanes and operation records
├── reducers and recovery
└── storage traits

pi-agent-harness
├── compaction and branch summarization
├── skills and prompt templates
├── reference tools
├── telemetry
└── orchestration over agent-core + session + environment

pi-agent-env
├── filesystem and process capability traits
└── portable environment types

pi-agent-runtime-tokio
├── Tokio environment implementation
├── Send actor facade
└── native process execution

pi-agent-compat-pi-jsonl
└── Pi v4 JSONL reader and constrained writer
```

Provider implementations remain separate leaf crates or features. Pi's provider factories intentionally import only their own catalog and lazy API wrapper, while the heavy `providers/all` entrypoint explicitly imports everything. The Rust analogue remains separate provider crates plus an optional all-providers aggregator.

---

# 1. Opaque provider metadata through streaming, assembly, persistence, and replay

## 1.1 What Pi actually does

Pi stores replay metadata in several overloaded fields:

* `ThinkingContent.thinkingSignature`
* `TextContent.textSignature`
* `ToolCall.thoughtSignature`
* `AssistantMessage.responseId`

The `AssistantMessageEvent` protocol itself does not have signature or opaque-data events. Instead, every delta event carries a mutable `partial: AssistantMessage`; API implementations silently mutate replay fields on that partial message. `thinkingSignature` is explicitly documented as provider-specific reasoning replay data, redacted thinking uses the same slot, and tool-call thought signatures are separately stored. `AssistantMessage` also stores response IDs and diagnostics. See `packages/ai/src/types.ts:300–540`.

That design works in JavaScript because all consumers observe the same mutable object. It is a poor fit for an immutable Rust event protocol and for event persistence or RPC. In particular, Anthropic's `signature_delta` mutates `thinkingSignature` without emitting any corresponding Pi stream event. See `packages/ai/src/api/anthropic-messages.ts:600–790`.

## 1.2 Deliberate Rust divergence: a replay envelope

Opaque replay data should be separate from the displayable canonical content, while retaining explicit links to the content block or provider output item it belongs to.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: MessageId,
    pub provider: ProviderId,
    pub api: ApiId,
    pub requested_model: ModelId,
    pub response_model: Option<ModelId>,
    pub response_id: Option<String>,

    pub content: Vec<ContentBlock>,
    pub replay: ReplayEnvelope,

    pub usage: Usage,
    pub finish: AssistantFinish,
    pub timestamp: Timestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayEnvelope {
    pub schema_version: u16,
    pub source: ReplayScope,

    /// Provider-output order, not merely canonical content-block order.
    pub items: Vec<ReplayItem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayScope {
    pub provider: ProviderId,
    pub api: ApiId,

    /// The model to which the request was made.
    pub requested_model: ModelId,

    /// The concrete model that produced the response, when reported.
    pub produced_by_model: ModelId,

    pub protocol_revision: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayItem {
    pub id: ReplayItemId,

    /// Original provider output ordering.
    pub ordinal: u32,

    pub target: ReplayTarget,

    /// Open string identifier so third-party API crates can add replay kinds.
    pub kind: ReplayKind,

    pub applicability: ReplayApplicability,
    pub completeness: ReplayCompleteness,
    pub payload: OpaquePayload,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplayTarget {
    Message,
    ContentBlock(ContentBlockId),
    ToolCall(ToolCallId),

    /// Used by APIs such as OpenAI Responses whose replay unit is an output item.
    ProviderOutputItem { output_index: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplayApplicability {
    ExactProviderApiModel,
    ExactProviderApi,
    ApiFamily,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplayCompleteness {
    Complete,
    Incomplete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OpaquePayload {
    Utf8(String),

    /// Serialized as base64 at JSON/FFI boundaries.
    Bytes(Vec<u8>),

    /// Exact JSON bytes produced by the compatibility serializer.
    ///
    /// This preserves key order and number/string representation for the
    /// second-request wire encoder.
    JsonBytes(Vec<u8>),
}
```

> Correction: Pinned Pi retains the calculated monetary cost on every assistant response as part of `usage.cost`, including after persistence. Because Part 1 §3.9 deliberately separates token `Usage` from monetary `Cost`, the Rust `AssistantMessage` and `AssistantMessageSnapshot` additionally carry `cost: Option<Cost>`; API-family decoders populate it at terminal assembly and `pi-agent-core` may aggregate only same-currency, fully known response costs (`packages/ai/src/types.ts:319–332`; `packages/ai/src/models.ts:878–897`; `packages/ai/src/api/openai-responses.ts:347–379`; `packages/ai/src/api/openai-codex-responses.ts:593–630`).

Two distinctions matter:

* **Canonical content** is what applications display, summarize, edit, and hand off.
* **Replay items** are provider-protocol artifacts used only by an encoder that understands their `kind`.

The opaque item is not interpreted by `pi-agent-core`.

### Why ordered message-level items are necessary

A block-local `ProviderOpaque` is still insufficient for OpenAI Responses. Its history is an ordered sequence of reasoning, message, and function-call output items. The encoder must preserve that provider output ordering, not merely group all thinking blocks before text and tool calls.

Canonical blocks therefore carry stable IDs:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContentBlock {
    Text {
        id: ContentBlockId,
        text: String,
    },
    Thinking {
        id: ContentBlockId,
        text: String,
        redacted: bool,
    },
    ToolCall {
        id: ContentBlockId,
        call: ToolCall,
    },
}
```

The replay envelope carries the provider ordering and points back to those IDs.

## 1.3 Revised streaming protocol

```rust
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum AssistantEvent {
    MessageStarted {
        message_id: MessageId,
        provider: ProviderId,
        api: ApiId,
        model: ModelId,
    },

    ResponseMetadata {
        response_id: Option<String>,
        response_model: Option<ModelId>,
    },

    ContentBlockStarted {
        block_id: ContentBlockId,
        content_index: u32,
        kind: ContentBlockKind,
    },

    TextDelta {
        block_id: ContentBlockId,
        delta: String,
    },

    ThinkingDelta {
        block_id: ContentBlockId,
        delta: String,
    },

    ToolCallMetadata {
        block_id: ContentBlockId,
        call_id: ToolCallId,
        name: Option<String>,
    },

    ToolArgumentsDelta {
        block_id: ContentBlockId,
        delta: String,
    },

    ReplayItemStarted {
        item_id: ReplayItemId,
        ordinal: u32,
        target: ReplayTarget,
        kind: ReplayKind,
        applicability: ReplayApplicability,
    },

    ReplayData {
        item_id: ReplayItemId,
        operation: ReplayDataOperation,
    },

    ReplayItemFinished {
        item_id: ReplayItemId,
    },

    ContentBlockFinished {
        block_id: ContentBlockId,
    },

    UsageUpdated {
        cumulative: Usage,
    },

    Finished {
        message: AssistantMessage,
    },

    Failed {
        message: AssistantMessage,
    },

    Cancelled {
        message: AssistantMessage,
    },
}

#[derive(Clone, Debug)]
pub enum ReplayDataOperation {
    ReplaceUtf8(String),
    AppendUtf8(String),
    ReplaceBytes(Vec<u8>),
    AppendBytes(Vec<u8>),
    ReplaceJsonBytes(Vec<u8>),
}
```

`AssistantAssembler` is the single authoritative consumer:

```rust
pub struct AssistantAssembler {
    message: AssistantMessageBuilder,
    blocks: IndexMap<ContentBlockId, BlockBuilder>,
    replay: IndexMap<ReplayItemId, ReplayItemBuilder>,
    terminal: bool,
}

impl AssistantAssembler {
    pub fn apply(&mut self, event: &AssistantEvent)
        -> Result<(), AssemblyError>;

    pub fn snapshot(&self) -> AssistantMessageSnapshot<'_>;

    pub fn finish_completed(
        self,
        finish: AssistantFinish,
    ) -> Result<AssistantMessage, AssemblyError>;

    pub fn finish_failed(
        self,
        error: PublicError,
    ) -> AssistantMessage;

    pub fn finish_cancelled(
        self,
        reason: CancellationReason,
    ) -> AssistantMessage;
}
```

> Correction: Pinned Pi assigns `rawStopReason` before mapping a provider finish reason and retains it when that mapping produces a failed assistant (including `content_filter`). The Rust assembler's failed-terminal operation therefore also accepts `raw_provider_reason: Option<String>`; provider decoders pass the observed value while transport failures pass `None` (`packages/ai/src/api/openai-completions.ts:535–545,641–674`; `packages/ai/test/openai-completions-raw-stop-reason.test.ts`).

> Correction: Pinned OpenAI Responses initializes a function-call scratch buffer from `response.output_item.added.item.arguments` and replaces it with the authoritative `response.output_item.done.item.arguments`, which need not extend the streamed prefix. The immutable Rust protocol therefore includes `ToolArgumentsReplaced { block_id, arguments }` in addition to `ToolArgumentsDelta`; decoders emit the initial value as a delta and use replacement when the final value is not an append-only extension (`packages/ai/src/api/openai-responses-shared.ts:650–760`).

> Correction: Pinned `pi-messages` treats the complete `toolCall` carried by `toolcall_end` as authoritative and replaces the streamed tool-call `id`, `name`, and `arguments` in place. Because `ToolCallMetadata` establishes stable incremental identity and rejects identity drift, the immutable Rust protocol also includes `ToolCallMetadataReplaced { block_id, call_id, name }` for this terminal provider-authoritative replacement (`packages/ai/src/api/pi-messages.ts`, `toolcall_end`).

Completion validation is strict:

```rust
impl AssistantAssembler {
    fn validate_successful_replay(&self) -> Result<(), AssemblyError> {
        for item in self.replay.values() {
            if !item.finished {
                return Err(AssemblyError::IncompleteReplayItem(item.id.clone()));
            }
        }
        Ok(())
    }
}
```

On failure or cancellation, unfinished items are retained as `ReplayCompleteness::Incomplete`; an encoder must never replay them.

---

## 1.4 Worked example: Anthropic Messages

### Pi behavior

Anthropic streams:

* visible thinking through `thinking_delta`;
* the signature through separate `signature_delta` events;
* redacted thinking as a `redacted_thinking` block whose opaque `data` is stored as the block's signature.

Pi initializes the thinking block at `content_block_start`, appends visible thinking deltas, and separately concatenates `signature_delta` into `thinkingSignature`. Redacted content is represented by display text `"[Reasoning redacted]"`, the opaque payload, and `redacted: true`. See `packages/ai/src/api/anthropic-messages.ts:600–790`.

On replay, Pi emits either:

```json
{
  "type": "thinking",
  "thinking": "...",
  "signature": "..."
}
```

or:

```json
{
  "type": "redacted_thinking",
  "data": "..."
}
```

An unsigned non-redacted thinking block is instead lowered to text unless the model compat flag explicitly permits an empty signature. See `packages/ai/src/api/anthropic-messages.ts:1200–1450`.

### Rust events: signed ordinary thinking

```text
ContentBlockStarted {
    block_id: b0,
    content_index: 0,
    kind: Thinking
}

ReplayItemStarted {
    item_id: r0,
    ordinal: 0,
    target: ContentBlock(b0),
    kind: "anthropic.messages.thinking-signature",
    applicability: ExactProviderApiModel
}

ThinkingDelta {
    block_id: b0,
    delta: "We need to inspect..."
}

ReplayData {
    item_id: r0,
    operation: AppendUtf8("EqQBCg...")
}

ReplayData {
    item_id: r0,
    operation: AppendUtf8("remaining-signature...")
}

ReplayItemFinished { item_id: r0 }
ContentBlockFinished { block_id: b0 }
```

The assembler produces:

```rust
ContentBlock::Thinking {
    id: b0,
    text: "We need to inspect...".into(),
    redacted: false,
}
```

and:

```rust
ReplayItem {
    id: r0,
    ordinal: 0,
    target: ReplayTarget::ContentBlock(b0),
    kind: ReplayKind::new("anthropic.messages.thinking-signature"),
    applicability: ReplayApplicability::ExactProviderApiModel,
    completeness: ReplayCompleteness::Complete,
    payload: OpaquePayload::Utf8("EqQBCg...remaining-signature...".into()),
}
```

### Rust events: redacted thinking

```text
ContentBlockStarted {
    block_id: b1,
    content_index: 1,
    kind: Thinking
}

ThinkingDelta {
    block_id: b1,
    delta: "[Reasoning redacted]"
}

ReplayItemStarted {
    item_id: r1,
    ordinal: 1,
    target: ContentBlock(b1),
    kind: "anthropic.messages.redacted-thinking",
    applicability: ExactProviderApiModel
}

ReplayData {
    item_id: r1,
    operation: ReplaceUtf8("<opaque-data>")
}

ReplayItemFinished { item_id: r1 }
ContentBlockFinished { block_id: b1 }
```

### Persisted representation

```json
{
  "type": "thinking",
  "id": "b0",
  "text": "We need to inspect...",
  "redacted": false,
  "replayItem": "r0"
}
```

```json
{
  "id": "r0",
  "ordinal": 0,
  "target": {
    "type": "content_block",
    "id": "b0"
  },
  "kind": "anthropic.messages.thinking-signature",
  "applicability": "exact_provider_api_model",
  "completeness": "complete",
  "payload": {
    "encoding": "utf8",
    "data": "EqQBCg...remaining-signature..."
  }
}
```

### Turn-two encoder

```rust
fn encode_anthropic_thinking(
    block: &ThinkingBlock,
    replay: &ReplayEnvelope,
    target: &ReplayScope,
    compat: &AnthropicMessagesCompat,
) -> Result<Option<AnthropicContentBlock>, EncodeError> {
    let item = replay.complete_item_for_block(
        block.id,
        "anthropic.messages.thinking-signature",
        target,
    );

    if block.redacted {
        let data = item
            .and_then(ReplayItem::as_utf8)
            .ok_or(EncodeError::MissingRedactedThinkingPayload)?;

        return Ok(Some(AnthropicContentBlock::RedactedThinking {
            data: data.to_owned(),
        }));
    }

    match item.and_then(ReplayItem::as_utf8) {
        Some(signature) if !signature.is_empty() => {
            Ok(Some(AnthropicContentBlock::Thinking {
                thinking: block.text.clone(),
                signature: signature.to_owned(),
            }))
        }

        Some(_) if compat.allow_empty_signature => {
            Ok(Some(AnthropicContentBlock::Thinking {
                thinking: block.text.clone(),
                signature: String::new(),
            }))
        }

        _ if !block.text.trim().is_empty() => {
            Ok(Some(AnthropicContentBlock::Text {
                text: block.text.clone(),
            }))
        }

        _ => Ok(None),
    }
}
```

> Correction: Pinned Pi replays a redacted thinking block from its redacted payload independently of an ordinary thinking signature, and treats a signature as usable only when `thinkingSignature.trim().length > 0`. The Rust encoder therefore looks up `anthropic.messages.redacted-thinking` for redacted blocks, looks up `anthropic.messages.thinking-signature` only for ordinary thinking, and treats whitespace-only signatures as empty before applying `allow_empty_signature` (`packages/ai/src/api/anthropic-messages.ts:1218–1249`).

This reconstructs Pi's same-provider turn-two request exactly, while making the signature observable and persistable during streaming.

---

## 1.5 Worked example: OpenAI-compatible Chat Completions

### Pi behavior

Some OpenAI-compatible endpoints return visible reasoning through fields such as:

* `reasoning_content`
* `reasoning`
* `reasoning_text`

Pi remembers the field name in `thinkingSignature`.

More importantly, endpoints such as OpenRouter can return a `reasoning_details` array. Pi appends each valid detail to an array, retains original order, and serializes the entire array into `thinkingSignature`. See `packages/ai/src/api/openai-completions.ts:500–790`.

On replay, Pi:

1. first looks for structured reasoning details in a thinking block signature;
2. falls back to encrypted reasoning details stored on tool-call thought signatures by older versions;
3. otherwise emits visible reasoning through the remembered reasoning field;
4. optionally converts visible thinking to ordinary text for incompatible endpoints.

See `packages/ai/src/api/openai-completions.ts:1050–1300`.

### Rust events

Suppose a delta contains:

```json
{
  "reasoning_details": [
    {
      "type": "reasoning.encrypted",
      "id": "rs_1",
      "data": "opaque-A"
    },
    {
      "type": "reasoning.summary",
      "id": "rs_2",
      "summary": "..."
    }
  ]
}
```

The API decoder emits:

```text
ReplayItemStarted {
    item_id: r0,
    ordinal: 0,
    target: ContentBlock(b0),
    kind: "openai.chat.reasoning-detail",
    applicability: ExactProviderApiModel
}
ReplayData {
    item_id: r0,
    operation: ReplaceJsonBytes(
        b"{\"type\":\"reasoning.encrypted\",\"id\":\"rs_1\",\"data\":\"opaque-A\"}"
    )
}
ReplayItemFinished { item_id: r0 }

ReplayItemStarted {
    item_id: r1,
    ordinal: 1,
    target: ContentBlock(b0),
    kind: "openai.chat.reasoning-detail",
    applicability: ExactProviderApiModel
}
ReplayData {
    item_id: r1,
    operation: ReplaceJsonBytes(
        b"{\"type\":\"reasoning.summary\",\"id\":\"rs_2\",\"summary\":\"...\"}"
    )
}
ReplayItemFinished { item_id: r1 }
```

The exact JSON bytes here are not necessarily the original response-wire bytes: Pi parses the provider object and then uses `JSON.stringify`. The parity target is therefore **the JSON representation Pi would place into the second request**, not a byte-preserved copy of the first response frame.

### Persisted representation

```json
{
  "id": "r0",
  "ordinal": 0,
  "target": {
    "type": "content_block",
    "id": "b0"
  },
  "kind": "openai.chat.reasoning-detail",
  "applicability": "exact_provider_api_model",
  "completeness": "complete",
  "payload": {
    "encoding": "json_bytes_base64",
    "data": "eyJ0eXBlIjoicmVhc29uaW5nLmVuY3J5cHRlZCIsImlkIjoicnNfMSIsImRhdGEiOiJvcGFxdWUtQSJ9"
  }
}
```

### Turn-two encoder

```rust
fn collect_openai_chat_reasoning_details(
    block: &ThinkingBlock,
    replay: &ReplayEnvelope,
    target: &ReplayScope,
) -> Result<Option<OrderedJsonArray>, EncodeError> {
    let items = replay
        .items_for_block(block.id)
        .filter(|item| item.kind == "openai.chat.reasoning-detail")
        .filter(|item| item.is_complete_and_applicable(target))
        .collect::<Vec<_>>();

    if items.is_empty() {
        return Ok(None);
    }

    let mut details = OrderedJsonArray::new();
    for item in items {
        details.push(parse_ordered_json(item.json_bytes()?)?);
    }
    Ok(Some(details))
}
```

The assistant message encoder then writes:

```json
{
  "role": "assistant",
  "content": "...",
  "reasoning_details": [
    { "type": "reasoning.encrypted", "id": "rs_1", "data": "opaque-A" },
    { "type": "reasoning.summary", "id": "rs_2", "summary": "..." }
  ]
}
```

with the same array order.

### Legacy fallback

The Rust Pi-session importer should understand old `ToolCall.thoughtSignature` values and convert them into replay items when no thinking-block reasoning details exist.

New Rust messages should **not** write that legacy representation.

---

## 1.6 Worked example: OpenAI Responses

### Pi behavior

OpenAI Responses stores several kinds of replay information:

* `response.created` sets `AssistantMessage.responseId`;
* a complete reasoning output item is serialized into a thinking block's signature;
* output-message IDs and optional phases are encoded in `textSignature`;
* function calls preserve both `call_id` and output-item ID in Pi's compound tool ID;
* the output item's position is tracked through `output_index`.

On turn two, Pi reconstructs reasoning output items from their serialized signatures, output-message IDs from text signatures, and function-call IDs from compound tool-call IDs. See `packages/ai/src/api/openai-responses-shared.ts:40–280`.

> Correction: Pinned Pi reconstructs Responses assistant input by walking the projected canonical content blocks in order and consulting each surviving replay identity in place; it does not emit every applicable replay record before canonical fallback blocks. After cross-model projection, reasoning and text identities can be removed while a deferred function-call identity survives, so canonical `[thinking, text, tool]` encodes as `[message, message, function_call]`. Rust preserves that canonical block order while retaining provider output ordinals in the persisted replay envelope (`packages/ai/src/api/openai-responses-shared.ts:218–293`).

During streaming, Pi creates slots by `output_index`; when a complete reasoning item arrives, it replaces the visible thinking and serializes the entire reasoning item. Message completion records the message ID and phase, while function-call completion finalizes arguments and namespace. See `packages/ai/src/api/openai-responses-shared.ts:560–790`.

> Correction: Pinned Pi treats completed Responses reasoning and text as authoritative assignments, even when the completed value is not an append-only extension of streamed deltas. The immutable Rust protocol therefore includes `ThinkingReplaced { block_id, thinking }` and `TextReplaced { block_id, text }`; Responses decoders use them for non-prefix final values. Codex also copies boolean `response.end_turn` into the assistant record, and pre-stream WebSocket fallback appends a persisted `provider_transport_failure` diagnostic before continuing over SSE, so `AssistantMessage` and `ResponseMetadata` retain end-turn metadata and the event protocol can append diagnostics (`packages/ai/src/api/openai-responses-shared.ts:680–707`; `packages/ai/src/api/openai-codex-responses.ts:330–363,717–749`; `packages/ai/src/utils/diagnostics.ts:1–45`).

> Correction: Pinned Codex cached-WebSocket continuation is derived by re-encoding the fully assembled assistant message, not by copying the terminal `response.output` array. The latter may omit tool calls and may contain non-canonical streaming-only fields. The Rust Codex transport therefore assembles `response.output_item.done` records by `output_index`, canonicalizes message and tool-call items using the same turn-two shapes, backfills terminal encrypted reasoning, and uses that canonical sequence with `previous_response_id` (`packages/ai/src/api/openai-codex-responses.ts:1390–1439,1521–1532`; `packages/ai/src/api/openai-responses-shared.ts:218–291`).

> Correction: Pinned Codex maps `response.failed` and top-level `error` events before the shared Responses decoder: a failed response exposes only its nested provider message, a top-level error accepts nested code/message fields and prefixes the failure with `Codex error:`, and neither path retains `response.status` as `rawStopReason`. Public OpenAI Responses retains the shared mapper behavior described above (`packages/ai/src/api/openai-codex-responses.ts:704–749`; `packages/ai/src/api/openai-responses-shared.ts:704–736`).

> Correction: Pinned Responses completion falls back to streamed reasoning when both final summary and content are empty, to streamed function arguments when final arguments are absent or empty, and to streamed custom-tool input when final input is absent. It backfills encrypted reasoning only from non-empty terminal content, and the shared decoder records `response.id` but does not copy `response.model` onto the assistant message. Rust preserves those exact finalization rules (`packages/ai/src/api/openai-responses-shared.ts:650–707,744–769`).

> Correction: Pinned Responses preserves a function/custom-tool namespace across a same-provider/API model change when that tool is deferred, while dropping a paired `fc_*` output-item ID. Function-call identity replay therefore uses `ExactProviderApi` applicability and the encoder applies the model-sensitive item-ID and namespace sub-rules; reasoning and output-message replay remain `ExactProviderApiModel` (`packages/ai/src/api/openai-responses-shared.ts:248–291`).

> Correction: Pinned Codex treats any mapped WebSocket event, including `codex.rate_limits`, as stream start, but still retries one nested `previous_response_not_found` error regardless of whether such an event preceded it. Its `onResponse` callback runs only for the SSE `fetch` path, not for WebSocket exchanges. Rust scans the WebSocket prelude for that continuation error without making later connection-limit failures pre-stream-retryable, and synthetic WebSocket responses suppress `ResponseObserver` (`packages/ai/src/api/openai-codex-responses.ts:294–414,704–749,1442–1539`; `packages/ai/test/openai-codex-stream.test.ts`).

> Correction: Pinned Responses increments its fallback `msg_pi_*` message counter only after a source message emits at least one wire item; empty user messages and assistant messages containing only unsigned thinking are skipped without consuming an index. Rust likewise advances the counter only after conversion emits output (`packages/ai/src/api/openai-responses-shared.ts:184–349`; `packages/ai/test/openai-responses-message-id.test.ts`).

> Correction: Pinned Codex normalizes an unknown terminal response status to an absent status, which the shared Responses finalizer treats as `stop`; public Responses retains its exhaustive unknown-status failure. The Codex SSE parser also dispatches only blank-line-terminated frames and discards an unterminated EOF tail. Rust applies both behaviors only to the Codex family (`packages/ai/src/api/openai-codex-responses.ts:740–758,765–822`; `packages/ai/src/api/openai-responses-shared.ts:704–736`).

> Correction: Pinned Codex records a typed-session sticky SSE fallback for any WebSocket transport failure, including a body failure after semantic stream start, while clearing that session's cached continuation. The current request remains failed after start, but the next request for the same typed session bypasses WebSocket. Rust performs the same state transition in both Send and Local body adapters (`packages/ai/src/api/openai-codex-responses.ts:333–363,930–949,1534–1539`).

> Correction: Pinned shared Responses recognizes only `response.completed` and `response.incomplete` as successful terminal events; `response.done` becomes terminal only after the Codex event mapper translates it. The shared decoder also deletes every `output_index` slot at `response.output_item.done`, so later text, reasoning, function-argument, and custom-input events for that slot are ignored. Rust applies the same family-specific terminal rule and post-completion behavior (`packages/ai/src/api/openai-responses-shared.ts:650–735`; `packages/ai/src/api/openai-codex-responses.ts:704–758`).

> Correction: Pinned public and Codex Responses price `flex` at one-half and `priority` at two, except GPT-5.5 priority at five-halves. Codex additionally resolves a response-echoed `default` tier back to the requested `flex` or `priority` tier. Rust applies those multipliers with checked integer/rational arithmetic after provider usage normalization; the decoder configuration therefore retains the original requested service tier independently of payload middleware (`packages/ai/src/api/openai-responses.ts:347–379`; `packages/ai/src/api/openai-codex-responses.ts:593–630`; `packages/ai/test/openai-codex-stream.test.ts`).

### Rust representation

OpenAI Responses requires an ordered sequence of replay anchors:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OpenAiResponsesReplay {
    ReasoningItem {
        block_id: ContentBlockId,
        item_json: Vec<u8>,
    },

    MessageIdentity {
        block_id: ContentBlockId,
        item_id: String,
        phase: Option<OpenAiMessagePhase>,
    },

    FunctionCallIdentity {
        tool_call_id: ToolCallId,
        call_id: String,
        item_id: Option<String>,
        namespace: Option<String>,
        item_type: OpenAiToolItemType,
    },
}
```

These values remain encoded as open `ReplayItem` records in the core data model; the enum is the API crate's typed view.

### Stream events

For a reasoning item at output index zero:

```text
ReplayItemStarted {
    item_id: r0,
    ordinal: 0,
    target: ProviderOutputItem { output_index: 0 },
    kind: "openai.responses.reasoning-item",
    applicability: ExactProviderApiModel
}

ContentBlockStarted {
    block_id: b0,
    content_index: 0,
    kind: Thinking
}

ThinkingDelta {
    block_id: b0,
    delta: "Inspecting the request..."
}

ReplayData {
    item_id: r0,
    operation: ReplaceJsonBytes(
        b"{\"id\":\"rs_123\",\"type\":\"reasoning\",...,\"encrypted_content\":\"...\"}"
    )
}

ReplayItemFinished { item_id: r0 }
ContentBlockFinished { block_id: b0 }
```

For an output message at output index one:

```text
ReplayItemStarted {
    item_id: r1,
    ordinal: 1,
    target: ProviderOutputItem { output_index: 1 },
    kind: "openai.responses.message-identity",
    applicability: ExactProviderApiModel
}

ContentBlockStarted {
    block_id: b1,
    content_index: 1,
    kind: Text
}

TextDelta {
    block_id: b1,
    delta: "I found the issue."
}

ReplayData {
    item_id: r1,
    operation: ReplaceJsonBytes(
        b"{\"id\":\"msg_123\",\"phase\":\"final_answer\",\"block_id\":\"b1\"}"
    )
}

ReplayItemFinished { item_id: r1 }
ContentBlockFinished { block_id: b1 }
```

For a function call at output index two:

```text
ReplayItemStarted {
    item_id: r2,
    ordinal: 2,
    target: ProviderOutputItem { output_index: 2 },
    kind: "openai.responses.function-call-identity",
    applicability: ExactProviderApiModel
}

ToolCallMetadata {
    block_id: b2,
    call_id: "call_123",
    name: Some("read_file")
}

ToolArgumentsDelta {
    block_id: b2,
    delta: "{\"path\":\"README.md\"}"
}

ReplayData {
    item_id: r2,
    operation: ReplaceJsonBytes(
        b"{\"call_id\":\"call_123\",\"item_id\":\"fc_456\",\"namespace\":null,\"type\":\"function_call\"}"
    )
}

ReplayItemFinished { item_id: r2 }
ContentBlockFinished { block_id: b2 }
```

### Turn-two reconstruction

The encoder walks `ReplayEnvelope.items` by `ordinal`:

```rust
fn encode_openai_responses_assistant(
    message: &AssistantMessage,
    target: &ReplayScope,
) -> Result<Vec<OrderedJsonValue>, EncodeError> {
    let mut output = Vec::new();

    for replay_item in message.replay.items.iter().sorted_by_key(|item| item.ordinal) {
        if !replay_item.is_complete_and_applicable(target) {
            continue;
        }

        match replay_item.kind.as_str() {
            "openai.responses.reasoning-item" => {
                output.push(parse_ordered_json(replay_item.json_bytes()?)?);
            }

            "openai.responses.message-identity" => {
                let identity: MessageIdentity =
                    decode_replay_json(replay_item.json_bytes()?)?;
                let block = message.text_block(identity.block_id)?;

                output.push(ordered_json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": block.text,
                        "annotations": []
                    }],
                    "status": "completed",
                    "id": identity.id,
                    "phase": identity.phase
                }));
            }

            "openai.responses.function-call-identity" => {
                let identity: FunctionCallIdentity =
                    decode_replay_json(replay_item.json_bytes()?)?;
                let call = message.tool_call(identity.tool_call_id)?;

                output.push(encode_responses_tool_call(call, identity)?);
            }

            _ => {}
        }
    }

    Ok(output)
}
```

The response ID remains persisted for diagnostics, deferred-response support, or APIs that explicitly use it. Pi's context replay in the cited converter is driven primarily by reasoning-item, text-item, and function-call identities; `responseId` is not a substitute for that ordered item data.

---

## 1.7 Worked example: Bedrock Converse Stream

### Pi behavior

For redacted Bedrock reasoning, Pi:

1. emits a visible redaction placeholder;
2. buffers each binary `redactedContent` chunk;
3. base64-encodes the concatenated bytes into `thinkingSignature`;
4. removes the binary scratch buffer before persistence.

See `packages/ai/src/api/bedrock-converse-stream.ts:650–860`.

> Correction: Pinned Pi can receive a reasoning signature before the first `redactedContent` chunk for the same block; at that transition it clears the accumulated signature and retains only the redacted payload. The immutable Rust protocol therefore also includes `ReplayItemDiscarded { item_id }`, allowing the decoder to supersede the started signature item before starting the redacted-reasoning item without leaving an incomplete or stale artifact (`packages/ai/src/api/bedrock-converse-stream.ts:635–657`).

On replay, it base64-decodes that signature and emits:

```text
reasoningContent.redactedContent = <bytes>
```

For non-redacted signed reasoning, it emits `reasoningText.text` and `reasoningText.signature`; if a signature is missing for a model that requires one, it falls back to plain text. See `packages/ai/src/api/bedrock-converse-stream.ts:900–1045`.

### Rust events

```text
ContentBlockStarted {
    block_id: b0,
    content_index: 0,
    kind: Thinking
}

ThinkingDelta {
    block_id: b0,
    delta: "[Reasoning redacted]"
}

ReplayItemStarted {
    item_id: r0,
    ordinal: 0,
    target: ContentBlock(b0),
    kind: "bedrock.converse.redacted-reasoning",
    applicability: ExactProviderApiModel
}

ReplayData {
    item_id: r0,
    operation: AppendBytes([0x01, 0x02, ...])
}

ReplayData {
    item_id: r0,
    operation: AppendBytes([0xAF, 0x33, ...])
}

ReplayItemFinished { item_id: r0 }
ContentBlockFinished { block_id: b0 }
```

The native persisted value is the original byte sequence. Base64 is only the JSON representation:

```json
{
  "payload": {
    "encoding": "bytes_base64",
    "data": "AQLvM..."
  }
}
```

### Turn-two encoder

```rust
let bytes = replay
    .complete_item_for_block(
        block.id,
        "bedrock.converse.redacted-reasoning",
        target,
    )
    .and_then(ReplayItem::as_bytes)
    .ok_or(EncodeError::MissingRedactedThinkingPayload)?;

ContentBlock::ReasoningContent {
    redacted_content: bytes.to_vec(),
}
```

This is less lossy than Pi's in-memory representation while producing the same wire bytes.

---

## 1.8 Worked example: Google Generative AI and Vertex

### Pi behavior

Google's `thoughtSignature` can appear on any response part, including text and function calls. It does **not** mean that the part is a thinking part. Pi treats `thought: true` as the thinking marker and preserves a thought signature on the exact part where it appeared. Empty signed text or thinking parts must also survive because the signature may be required on the next turn.

Pi only replays signatures for the same provider and model and validates their base64 shape. See `packages/ai/src/api/google-shared.ts:1–220`.

### Rust events: tool-call signature

```text
ReplayItemStarted {
    item_id: r0,
    ordinal: 0,
    target: ToolCall(call_123),
    kind: "google.genai.thought-signature",
    applicability: ExactProviderApiModel
}

ReplayData {
    item_id: r0,
    operation: ReplaceUtf8("base64-signature==")
}

ReplayItemFinished { item_id: r0 }

ToolCallMetadata {
    block_id: b0,
    call_id: call_123,
    name: Some("read_file")
}

ToolArgumentsDelta {
    block_id: b0,
    delta: "{\"path\":\"README.md\"}"
}
```

### Rust events: empty signed text part

```text
ContentBlockStarted {
    block_id: b1,
    content_index: 1,
    kind: Text
}

ReplayItemStarted {
    item_id: r1,
    ordinal: 1,
    target: ContentBlock(b1),
    kind: "google.genai.thought-signature",
    applicability: ExactProviderApiModel
}

ReplayData {
    item_id: r1,
    operation: ReplaceUtf8("another-signature==")
}

ReplayItemFinished { item_id: r1 }
ContentBlockFinished { block_id: b1 }
```

The block's text remains empty, but the block itself is retained because it has a replay item.

### Turn-two encoder

```rust
fn google_signature_for_target(
    replay: &ReplayEnvelope,
    target: ReplayTarget,
    request_scope: &ReplayScope,
) -> Option<&str> {
    replay
        .complete_item(
            target,
            "google.genai.thought-signature",
            request_scope,
        )
        .and_then(ReplayItem::as_utf8)
        .filter(|value| is_valid_base64(value))
}
```

The encoder attaches it to the same `Part`:

```rust
Part {
    function_call: Some(FunctionCall {
        id: Some(call.id.to_string()),
        name: call.name.clone(),
        args: call.arguments.clone(),
    }),
    thought_signature: google_signature_for_target(
        replay,
        ReplayTarget::ToolCall(call.id),
        target_scope,
    )
    .map(str::to_owned),
    ..Default::default()
}
```

It must never move the signature to a neighboring thinking or text block.

---

## 1.9 Replay invariants

These should be enforced by tests and debug assertions:

```text
R1. Every complete replay item has a stable id, target, kind, scope, and ordinal.

R2. A successful terminal message contains no incomplete replay item.

R3. Failed/cancelled terminal messages may contain incomplete replay items,
    but encoders ignore them.

R4. Same-provider replay is deterministic after:
        event assembly
        → serialization
        → deserialization
        → encoding.

R5. Cross-provider projection never passes opaque replay items unless an API-family
    implementation explicitly declares them portable.

R6. A signature attached to a provider part remains attached to that exact
    canonical target.

R7. Provider output ordering is retained independently of canonical block ordering.

R8. Scratch parsing state—partial JSON, SDK indexes, binary chunk vectors—is never
    part of the persisted public message schema.
```

The primary proof fixture is:

```rust
let message_1 = assemble(captured_events)?;
let bytes = serde_json::to_vec(&message_1)?;
let restored: AssistantMessage = serde_json::from_slice(&bytes)?;
let request_2 = api.encode_context(&[restored], &target)?;

assert_eq!(request_2.body, expected_pi_turn_two_body);
```

---

# 2. Failure, cancellation, retry ownership, and middleware

## 2.1 Failed and cancelled messages are committed transcript records

Pi's `StreamFunction` contract says request, model, and runtime failures should be encoded in the stream, with a final `AssistantMessage` whose stop reason is `error` or `aborted` and whose `errorMessage` is populated. See `packages/ai/src/types.ts:300–540`.

The agent loop inserts the partial assistant message into context on `start`, replaces it as updates arrive, and replaces it with the terminal message on either `done` or `error`. The terminal message is therefore committed before `turn_end` and `agent_end`. See `packages/agent/src/agent-loop.ts:1–420`.

The Rust model should make this normative:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantFinish {
    pub reason: AssistantFinishReason,
    pub raw_provider_reason: Option<String>,
    pub error: Option<PublicError>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum AssistantFinishReason {
    Stop,
    Length,
    ToolUse,
    Deferred,
    Error,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub provider_code: Option<String>,
    pub status: Option<u16>,
    pub request_id: Option<String>,
}
```

The earlier `RunOutcome::Failed { partial }` shape should be replaced:

```rust
pub enum RunOutcome {
    Completed {
        final_message_id: MessageId,
        usage: Usage,
        cost: Option<Cost>,
    },

    Failed {
        /// Already committed to AgentState.transcript.
        committed_message_id: MessageId,
        error: PublicError,
    },

    Cancelled {
        /// Already committed to AgentState.transcript.
        committed_message_id: MessageId,
        reason: CancellationReason,
    },
}
```

There is no separate uncommitted "partial" at the agent boundary. The final errored or aborted assistant record is the partial content plus terminal metadata.

### Exact failed record

```rust
AssistantMessage {
    id: same_message_id_used_during_streaming,
    content: all_text_thinking_and_finalized_tool_fragments_seen_so_far,
    replay: ReplayEnvelope {
        complete_items: preserved,
        incomplete_items: marked_incomplete,
    },
    usage: last_known_cumulative_usage,
    response_id: last_known_response_id,
    response_model: last_known_response_model,
    finish: AssistantFinish {
        reason: AssistantFinishReason::Error,
        raw_provider_reason: provider_reason_if_any,
        error: Some(PublicError {
            code: normalized_error_code,
            message: sanitized_provider_error_text,
            retryable,
            provider_code,
            status,
            request_id,
        }),
    },
}
```

### Exact cancelled record

```rust
AssistantMessage {
    // Same preservation rules as failure.
    finish: AssistantFinish {
        reason: AssistantFinishReason::Aborted,
        raw_provider_reason: None,
        error: Some(PublicError {
            code: "cancelled".into(),
            message: cancellation_display_text,
            retryable: false,
            provider_code: None,
            status: None,
            request_id: last_known_request_id,
        }),
    },
    ..
}
```

The cancellation display message can reproduce Pi's provider-specific wording in compatibility mode. Native callers should rely on `code == "cancelled"` rather than parsing the text.

### Failure before a provider stream starts

At the low-level model API, an invalid model reference or malformed request may still return:

```rust
Result<AssistantStream, RequestStartError>
```

At the agent level, once the user prompt has been committed, a failed run should still close with an empty-content failed assistant message. That gives every attempted model turn a structurally complete transcript record.

## 2.2 What the next turn sees

After a failed request, the durable transcript is:

```text
... prior messages
UserMessage
AssistantMessage {
    finish.reason = Error,
    partial content preserved
}
```

Pi's provider-side `transformMessages` skips assistant messages whose stop reason is `error` or `aborted`, because replaying partial reasoning or incomplete output items can make provider APIs reject the history. See `packages/ai/src/api/transform-messages.ts:1–380`.

The Rust design should preserve the same distinction:

```text
Durable transcript:
    [..., user, failed_assistant]

Provider request projection:
    [..., user]
```

That projection is produced by `ContextPolicy`, not by deleting the failed record.

If the user submits a new prompt after the failed turn, the durable transcript becomes:

```text
[..., user_1, failed_assistant, user_2]
```

and the provider projection is:

```text
[..., user_1, user_2]
```

The API encoder may group or bridge consecutive user messages if the target API requires it, but the durable history remains unchanged.

## 2.3 `continue()` versus retry

Pi's low-level `agentLoopContinue` rejects a context whose last message is an assistant message. The high-level `Agent.continue()` likewise rejects an assistant tail, except that it first drains queued steering or follow-up messages. See `packages/agent/src/agent-loop.ts:1–420` and `packages/agent/src/agent.ts:1–460`.

This creates tension with README wording that describes `continue()` as useful after errors. The actual code contract wins.

I would preserve `continue()`'s strict precondition and add an explicit operation:

```rust
impl Agent {
    /// Continue only when the durable tail is user or tool-result.
    pub fn continue_run(&mut self, cancel: CancellationToken)
        -> AgentEventStream<'_>;

    /// Retry the logical model turn after an Error/Aborted assistant.
    ///
    /// The failed record remains durable. Context projection excludes it.
    pub fn retry_last_turn(&mut self, cancel: CancellationToken)
        -> AgentEventStream<'_>;
}
```

This is a **deliberate divergence** from Pi's public surface. It removes an ambiguity without losing Pi's transcript behavior.

## 2.4 Retry ownership

Pi's retry helper reproduces the pinned OpenAI/Anthropic SDK policy while making the wait interruptible:

* `x-should-retry: true/false` overrides classification;
* network errors without a status are retryable;
* 408, 409, 429, and 5xx are retryable;
* `retry-after-ms` takes precedence;
* `Retry-After` accepts either seconds or an HTTP date;
* exponential delay starts at 500 ms, caps at 8 seconds, and applies downward jitter;
* provider-requested waits longer than `maxRetryDelayMs` fail immediately;
* the default maximum provider wait is 60 seconds;
* the cancellation signal interrupts both the request and the backoff.

See `packages/ai/src/utils/provider-retry.ts:1–260`.

> Correction: Pinned Pi's implementation retries every numeric status `>= 500`, not only statuses in the HTTP 5xx range. The Rust classifier preserves that exact predicate (`packages/ai/src/utils/provider-retry.ts:22–36`).

> Correction: Pinned Codex Responses does not use the generic OpenAI retry classifier. It defaults to zero retries. When retries are enabled, status 429/500/502/503/504 and the transient raw-response-text allowlist are handled first with `retry-after-ms`/`Retry-After` support and the configured maximum server delay; named account/quota/billing 429 responses are terminal. Every other HTTP body is then parsed through `parseErrorResponse`, and the surrounding catch applies its case-sensitive `"usage limit"` exclusion to that parsed friendly/error message, not to the raw JSON; those catch-path retries ignore response delay headers. Network failures use the same case-sensitive exclusion. Backoff is an unjittered one-second exponential base. The Codex provider registrations therefore install a family-specific Send/Local classifier and policy, retain raw response text for initial classification, and normalize the terminal public failure through the classifier's object-safe Send/Local terminal hook (`packages/ai/src/api/openai-codex-responses.ts:44–48,113–167,377–451,1549–1574`).

### Correct owner

The retry loop belongs inside the **API transport implementation**, immediately around the operation that establishes the HTTP response or provider SDK stream.

It does not belong in:

* `pi-agent-core`;
* generic `ModelRuntime`;
* context middleware;
* an agent-level run retry;
* a response stream after semantic deltas have already been emitted.

Provider registration can supply defaults or a classifier override, but the API implementation owns execution because only it knows when retrying is safe.

```rust
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub max_server_delay: Option<Duration>,

    pub exponential_base: Duration,
    pub exponential_cap: Duration,

    /// For Pi parity: 0.75..=1.0 multiplier.
    pub jitter_multiplier: RangeInclusive<f64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            max_server_delay: Some(Duration::from_secs(60)),
            exponential_base: Duration::from_millis(500),
            exponential_cap: Duration::from_secs(8),
            jitter_multiplier: 0.75..=1.0,
        }
    }
}

pub trait RetryClassifier: Send + Sync {
    fn classify(
        &self,
        failure: &AttemptFailure,
        policy: &RetryPolicy,
    ) -> RetryDecision;
}

pub enum RetryDecision {
    DoNotRetry,
    RetryAfter(Duration),
    RejectServerDelay {
        requested: Duration,
        maximum: Duration,
    },
}
```

The API handler receives the resolved policy:

```rust
pub struct ApiExecutionContext<'a> {
    pub model: &'a ModelDescriptor,
    pub endpoint: &'a Url,
    pub headers: &'a HeaderMap,
    pub retry_policy: &'a RetryPolicy,
    pub retry_classifier: &'a dyn RetryClassifier,
    pub transport: &'a dyn HttpTransport,
    pub cancellation: &'a CancellationToken,
}
```

> Correction: Pinned OpenAI Completions decoding closes streamed custom grammar-tool input with the input-property name derived from the request's configured tool schema. The execution context must therefore retain the canonical request context (or an equivalent request-scoped decoder configuration) through response decoding; the Rust `ApiExecutionContext` and local counterpart include `context: &Context` for that purpose (`packages/ai/src/api/openai-completions.ts:290–490`; `packages/ai/src/api/constrained-sampling.ts:250–277`).

> Correction: Pinned Codex Responses pricing must distinguish a response-echoed `service_tier: "default"` from the caller's requested `flex` or `priority`, and request payload middleware must not silently change the pricing contract after lowering. The Rust `ApiExecutionContext` and local counterpart therefore also retain a borrowed `ApiCallOptions` view of the original simple or full call options for request-scoped decoder configuration (`packages/ai/src/api/openai-codex-responses.ts:623–630,655–665,1508–1519`; `packages/ai/test/openai-codex-stream.test.ts`).

> Correction: Pinned Bedrock request encoding consumes provider-environment values with request-scoped-over-ambient precedence, including region, `PI_CACHE_RETENTION`, and `AWS_BEDROCK_FORCE_CACHE`. `getProviderEnvValue` uses JavaScript truthiness, so every non-empty value, including whitespace-only text, is present and prevents ambient fallback. Because the portable core does not read process globals and auth resolution precedes API lowering, the Rust `ApiExecutionContext` and local counterpart also retain borrowed credential-derived invariant headers. The Bedrock leaf uses that non-wire channel to carry the resolved request decisions into encoding; mutable logical-header overlays cannot replace it (`packages/ai/src/api/bedrock-converse-stream.ts:792–840,1156,1206–1227`; `packages/ai/src/utils/provider-env.ts:45–50`).

### Cancellable retry loop

```rust
async fn establish_with_retry<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn RetryClassifier,
    cancellation: &CancellationToken,
    mut attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptFailure>>,
{
    let mut retry_index = 0;

    loop {
        cancellation.check()?;

        match attempt(retry_index).await {
            Ok(value) => return Ok(value),

            Err(error) => {
                cancellation.check()?;

                if retry_index >= policy.max_retries {
                    return Err(error);
                }

                let delay = match classifier.classify(&error, policy) {
                    RetryDecision::DoNotRetry => return Err(error),
                    RetryDecision::RetryAfter(delay) => delay,
                    RetryDecision::RejectServerDelay { requested, maximum } => {
                        return Err(AttemptFailure::RetryDelayTooLong {
                            requested,
                            maximum,
                            source: Box::new(error),
                        });
                    }
                };

                cancellable_sleep(delay, cancellation).await?;
                retry_index += 1;
            }
        }
    }
}
```

The operation is retryable only until the API handler exposes the first semantic event. A connection failure after text has been emitted terminates the stream with a failed `AssistantMessage`; it does not transparently restart and duplicate content.

## 2.5 Middleware contracts

Pi's shared options support:

* injected `fetch`;
* `onPayload`, which may mutate the payload in place or return a replacement;
* `onResponse`, invoked after receiving status and headers;
* request headers;
* retry and timeout values.

See `packages/ai/src/types.ts:80–300`.

Pi's Models-level `transformHeaders` runs after provider auth, model headers, and explicit request headers, but before provider dispatch.

Those are four separate concepts in Rust.

### Transport injection

```rust
pub trait HttpTransport: Send + Sync {
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>>;
}
```

This is the equivalent of injected `fetch`. WebSocket, Smithy/AWS, or native SDK APIs can expose different transport traits where HTTP request injection is not meaningful.

### Header transformation

```rust
pub trait HeaderTransform: Send + Sync {
    fn transform(
        &self,
        context: HeaderTransformContext<'_>,
        headers: &mut HeaderMap,
    ) -> BoxFuture<'_, Result<(), MiddlewareError>>;
}
```

`HeaderMap` must merge names case-insensitively and support explicit deletion markers before finalization.

### Typed payload transformation

```rust
pub trait PayloadTransform<A: ApiFamily>: Send + Sync {
    fn transform(
        &self,
        context: PayloadTransformContext<'_, A>,
        payload: &mut A::WireRequest,
    ) -> BoxFuture<'_, Result<PayloadTransformResult<A::WireRequest>, MiddlewareError>>;
}

pub enum PayloadTransformResult<T> {
    /// Retain the possibly in-place-mutated value.
    Continue,

    /// Replace it entirely.
    Replace(T),
}
```

At the dynamically dispatched boundary, the API-family adapter erases this trait into:

```rust
pub trait ErasedPayloadTransform: Send + Sync {
    fn transform(
        &self,
        context: ErasedPayloadContext<'_>,
        payload: &mut ProviderPayload,
    ) -> BoxFuture<'_, Result<PayloadTransformDisposition, MiddlewareError>>;
}
```

### Response observer

```rust
pub trait ResponseObserver: Send + Sync {
    fn on_response(
        &self,
        context: ResponseObservationContext<'_>,
        response: &ProviderResponseMetadata,
    ) -> BoxFuture<'_, Result<(), MiddlewareError>>;
}

pub struct ProviderResponseMetadata {
    pub attempt: u32,
    pub status: u16,
    pub headers: HeaderMap,
    pub request_id: Option<String>,
}
```

By default, `on_response` runs once for every actual HTTP response, including a response that causes a retry.

### Logical payload versus retry-attempt middleware

I would make one additional distinction that Pi does not expose clearly:

```rust
pub trait AttemptMiddleware: Send + Sync {
    fn before_attempt(
        &self,
        attempt: u32,
        request: &mut HttpRequest,
    ) -> BoxFuture<'_, Result<(), MiddlewareError>>;
}
```

* `PayloadTransform` runs once for the logical provider request.
* `AttemptMiddleware` runs for every retry attempt.
* The encoded body is otherwise frozen across retries.

That prevents an `onPayload` callback that generates IDs or mutates arrays from producing a different semantic request on each retry.

## 2.6 Full request ordering

The exact request pipeline should be:

```text
1. Resolve ModelRef to ModelDescriptor and ProviderRegistration.

2. Resolve stored/ambient/provider authentication.
   This may refresh OAuth under the credential-store lock.

3. Resolve effective base URL.
   Auth is allowed to supply a credential-specific base URL.

4. Construct logical headers:
      provider/auth headers
      → model headers
      → explicit request headers

5. Apply Models-level HeaderTransform in registration order.

6. Resolve API-family compatibility from:
      effective base URL defaults
      → typed model compat overrides

7. Lower SimpleGenerationOptions to API-family options.

8. Project and transform canonical context for the target model,
   producing a HandoffReport.

9. Encode the API-family wire request.

10. Run PayloadTransform middleware in registration order.
    Each transform sees prior mutations/replacements.

11. For each transport attempt:
      a. clone/freeze logical request;
      b. run AttemptMiddleware;
      c. send through injected transport or SDK;
      d. invoke ResponseObserver after status/headers arrive;
      e. classify pre-stream failures and retry if allowed.

12. Decode the response into normalized AssistantEvents.

13. After the first semantic event is visible, all subsequent failure is terminal
    and in-band; there is no transparent full-request retry.
```

> Correction: Pinned Google Vertex full options allow `project` and `location` to provide the request-scoped ADC/client scope even when stored and ambient configuration do not provide it. For a fully API-specific call, an API handler may therefore project auth-scoping fields from its typed full options before step 2; this projection is not simple-option lowering, and the original typed options still control later request routing (`packages/ai/src/api/google-vertex.ts:46–55,437–455`).

> Correction: Pinned Pi's Codex SSE transport applies model and caller headers first, then reasserts `Authorization`, `chatgpt-account-id`, `originator`, and `User-Agent`, followed by its SSE protocol headers. The Codex transport must therefore treat its auth/account/originator and protocol headers as final transport invariants rather than ordinary logical defaults (`packages/ai/src/api/openai-codex-responses.ts:1593–1631`).

> Correction: When Codex session affinity is enabled, pinned Pi derives its internal WebSocket cache, continuation, and sticky-fallback key only from typed `options.sessionId` plus cache retention, never from merged HTTP headers. It retains the raw typed session for that internal key, separately clamps the protocol-facing prompt/session identifier, and sets option-derived `session-id` and `x-client-request-id` after model and caller headers. The SSE transport therefore carries typed session state independently of hostile logical overlays and reasserts only the clamped protocol values at the final transport boundary in both Send and Local paths (`packages/ai/src/api/openai-codex-responses.ts:267–288,930–949,1466–1486,1614–1631`).

> Correction: Pinned public OpenAI Responses accepts a non-empty case-insensitive `Authorization` or `CF-AIG-Authorization` header when no separate API key resolves only when that header came from the caller's explicit option headers; model defaults or later transforms cannot grant initial auth eligibility. `Models` and `LocalModels` therefore require explicit-option eligibility before entering the logical header pipeline, allow this mode only for `openai-responses`, and still reject the request if neither header remains non-empty after final transforms (`packages/ai/src/api/openai-responses.ts:35–47,198–215,218–265`).

> Correction: Pinned public Responses applies option-derived session-affinity headers after model headers and before explicit request headers for both simple and full entrypoints. Rust delays full Responses affinity until the model-header overlay has completed; explicit request headers and final transforms remain later (`packages/ai/src/api/openai-responses.ts:218–289`).

### Bedrock signing special case

Pi inserts caller headers at the Smithy build step, after serialization but before SigV4 signing, and suppresses reserved `x-amz-*`, `authorization`, and `host` values. It also captures response headers using a deserialize middleware. See `packages/ai/src/api/bedrock-converse-stream.ts:120–330` and `320–500`.

> Correction: Pinned Pi suppresses those reserved Bedrock headers silently; it does not report their names through diagnostics. Rust parity therefore inserts allowed logical headers at the signer-compatible build stage and silently suppresses reserved names (`packages/ai/src/api/bedrock-converse-stream.ts:452–485`; `packages/ai/test/bedrock-custom-headers.test.ts`).

> Correction: Pinned Pi resolves `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` for the effective request target before constructing the Bedrock client, with request-scoped lowercase/uppercase aliases ahead of ambient aliases, and selects an HTTP/1 proxy-capable handler whenever a proxy applies. The injected Rust signer therefore receives the resolved request-scoped proxy URL and HTTP/1 requirement rather than consulting process globals itself (`packages/ai/src/api/bedrock-converse-stream.ts:207–219`; `packages/ai/src/utils/node-http-proxy.ts`; `packages/ai/test/node-http-proxy.test.ts`).

> Correction: The private Rust auth-to-signer carrier is not a new reserved logical header. It is suppressed only while its value remains the untouched credential-derived carrier; a model, explicit-request, or `HeaderTransform` overlay using that name is forwarded like every other non-reserved caller header (`packages/ai/src/api/bedrock-converse-stream.ts:440–473`; `packages/ai/test/bedrock-custom-headers.test.ts`).

The generic header pipeline therefore produces **logical headers**. The Bedrock transport adapter is responsible for inserting them at the signer-compatible stage and reporting any suppressed names through diagnostics.

> Correction: The preceding reporting clause is superseded: pinned Pi silently suppresses reserved Bedrock header names and emits no suppression diagnostic. The adapter inserts only allowed logical headers before signing (`packages/ai/src/api/bedrock-converse-stream.ts:452–485`; `packages/ai/test/bedrock-custom-headers.test.ts`).

---

# 3. Simple-to-API-specific lowering

## 3.1 Ownership

Pi's simple-option layer performs real planning:

* estimates context tokens;
* reserves a 4096-token safety margin;
* clamps output tokens to the context window;
* merges model sampling defaults with request sampling values;
* maps simple reasoning levels;
* calculates token-based thinking budgets;
* reserves at least 1024 tokens for the answer.

See `packages/ai/src/api/simple-options.ts:1–360`.

Then each API implementation performs its own family-specific lowering. Anthropic chooses adaptive effort versus token budget, applies model thinking maps, and further clamps thinking budget against max output. See `packages/ai/src/api/anthropic-messages.ts:800–1030`.

OpenAI Completions resolves its reasoning effort and later uses URL/model compatibility settings to choose request fields. See `packages/ai/src/api/openai-completions.ts:500–790`.

The Rust ownership should be:

```text
Models::stream_simple
    owns model/provider/auth/effective-endpoint orchestration
        ↓
ApiFamily::lower_simple
    owns family/model-specific planning
        ↓
ApiFamily::encode
    owns provider wire shape
```

Not:

```text
Models::stream_simple
    contains a global switch with every provider's reasoning rules
```

## 3.2 API-family trait

```rust
pub trait ApiFamily: Send + Sync + 'static {
    const API_ID: &'static str;

    type Compat: Clone + Send + Sync + Serialize + DeserializeOwned;
    type ModelConfig: Clone + Send + Sync;
    type FullOptions: Clone + Send + Sync;
    type OptionsPatch: Clone + Send + Sync + Default;
    type WireRequest: Send + Sync;

    fn resolve_compat(
        effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError>;

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError>;

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError>;
}
```

```rust
pub struct SimpleLoweringContext<'a, A: ApiFamily> {
    pub model: &'a TypedModelDescriptor<A>,
    pub compat: &'a A::Compat,
    pub effective_base_url: &'a Url,
    pub estimated_input_tokens: u64,
    pub available_context_tokens: u64,
}
```

The dynamic registry stores an erased wrapper:

```rust
pub trait ErasedApiHandler: Send + Sync {
    fn api_id(&self) -> &ApiId;

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError>;

    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream;
}
```

## 3.3 Typed options and `extensions[api]`

The prior proposal allowed both:

```rust
SimpleGenerationOptions.extensions[api]
```

and a typed `ApiOptions` value. Allowing both with vague precedence is a mistake.

They should be alternate representations of one API-specific patch.

```rust
pub enum ApiOptionsInput<A: ApiFamily> {
    None,
    Typed(A::OptionsPatch),
    Erased(ErasedApiOptionsPatch),
}
```

Typed Rust callers use:

```rust
models.stream_simple_typed::<AnthropicMessages>(
    model_ref,
    context,
    simple,
    AnthropicSimplePatch {
        thinking_display: Some(AnthropicThinkingDisplay::Summarized),
        ..Default::default()
    },
).await?;
```

FFI or config-file callers use:

```json
{
  "apiOptions": {
    "api": "anthropic-messages",
    "schemaVersion": 1,
    "value": {
      "thinkingDisplay": "summarized"
    }
  }
}
```

If a mixed builder somehow receives both, it fails:

```rust
LoweringError::ConflictingApiOptions {
    api: ApiId::new("anthropic-messages"),
}
```

There is no "typed wins" or "extension wins" rule.

### Precedence

For a simple call:

```text
catalog/model defaults
    < provider factory defaults
    < common simple options
    < one API-family options patch
    < onPayload wire transformation
```

For a fully API-specific call:

```rust
models.stream_api::<AnthropicMessages>(
    model_ref,
    context,
    AnthropicOptions { ... },
).await?;
```

there is no simple lowering. This is the equivalent of Pi's `stream` rather than `streamSimple`.

## 3.4 Common planning

```rust
pub struct CommonSimplePlan {
    pub max_output_tokens: u32,
    pub sampling: SamplingPlan,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub tool_choice: ToolChoice,
    pub reasoning: Option<ReasoningLevel>,
}

fn plan_common(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    estimator: &dyn TokenEstimator,
) -> Result<CommonSimplePlan, LoweringError> {
    const CONTEXT_SAFETY_TOKENS: u64 = 4096;

    let estimated = estimator.estimate(context)?;
    let available = model
        .limits
        .context_window
        .saturating_sub(estimated)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);

    let requested = simple
        .max_output_tokens
        .unwrap_or(model.limits.max_output_tokens);

    let max_output_tokens = requested
        .min(model.limits.max_output_tokens)
        .min(available.max(1) as u32);

    Ok(CommonSimplePlan {
        max_output_tokens,
        sampling: merge_sampling(
            &model.sampling_defaults,
            &simple.sampling,
        ),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
        tool_choice: simple.tool_choice.clone().unwrap_or_default(),
        reasoning: simple.reasoning,
    })
}
```

> Correction: Pinned Pi bypasses context estimation and clamping when the catalog context window is nonpositive. For a positive window it clamps the caller's requested/default output only to the remaining context (with a minimum of one), not separately to the catalog maximum; an explicit request may therefore exceed `model.maxTokens` when context permits. Rust preserves those predicates (`packages/ai/src/api/simple-options.ts:11–18,34`).

> Correction: Pinned Pi keeps model/request `samplingParams` separate from named simple fields, merges request keys over model keys, and applies that object after named OpenAI-family request fields. Thus model `samplingParams.temperature = 1` overrides named request `temperature = 0` unless request `samplingParams` supplies a later value. Rust preserves this ordered overlay (`packages/ai/src/api/simple-options.ts:21–34`; `packages/ai/src/api/openai-completions.ts:942–945`; `packages/ai/test/sampling-options.test.ts`).

> Correction: Pinned Responses simple adapters clamp to a reasoning-level name and pass that name to the full encoder; the full encoder then applies `thinkingLevelMap` exactly once. A missing or null mapped full-option value falls back to the requested string, while a public summary-only request uses literal `medium` without mapping it. Rust lowering therefore retains the clamped level name instead of eagerly replacing it with the mapped provider string, and both public and Codex full encoders perform the single applicable map (`packages/ai/src/api/openai-responses.ts:196–205,266–305`; `packages/ai/src/api/openai-codex-responses.ts:471–493,518–569`).

For Pi wire parity, the initial implementation should use Pi-equivalent token estimation rather than introducing a tokenizer-specific estimator that would change output caps.

## 3.5 Anthropic lowering

```rust
#[derive(Clone, Debug)]
pub struct AnthropicOptions {
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking: AnthropicThinking,
    pub thinking_display: AnthropicThinkingDisplay,
    pub tool_choice: ToolChoice,
    pub cache_retention: CacheRetention,
    pub metadata_user_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AnthropicThinking {
    Disabled,
    Adaptive {
        effort: Option<AnthropicEffort>,
    },
    Budget {
        budget_tokens: u32,
    },
}
```

```rust
impl ApiFamily for AnthropicMessages {
    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &AnthropicSimplePatch,
    ) -> Result<AnthropicOptions, LoweringError> {
        let common = plan_common(
            context.model.erased(),
            simple.context(),
            simple,
            simple.token_estimator(),
        )?;

        let requested_level = common.reasoning;
        let mapped_level = requested_level
            .map(|level| {
                context
                    .model
                    .thinking_levels
                    .resolve(level)
            })
            .transpose()?;

        let thinking = match mapped_level {
            None | Some(AnthropicThinkingValue::Off) => {
                AnthropicThinking::Disabled
            }

            Some(AnthropicThinkingValue::Effort(effort))
                if context.compat.force_adaptive_thinking =>
            {
                AnthropicThinking::Adaptive {
                    effort: Some(effort),
                }
            }

            Some(AnthropicThinkingValue::Effort(effort)) => {
                let budget = simple
                    .thinking_budgets
                    .budget_for(requested_level.unwrap())
                    .unwrap_or_else(|| default_budget(requested_level.unwrap()));

                let expanded_ceiling = simple
                    .max_output_tokens
                    .map(|answer| answer.saturating_add(budget))
                    .unwrap_or(context.model.common.limits.max_output_tokens)
                    .min(context.model.common.limits.max_output_tokens);

                let context_clamped = clamp_to_context(
                    expanded_ceiling,
                    context.available_context_tokens,
                );

                let budget = budget.min(context_clamped.saturating_sub(1024));

                let _ = effort; // Effort is used only by adaptive models.
                AnthropicThinking::Budget {
                    budget_tokens: budget,
                }
            }
        };

        let thinking_enabled =
            !matches!(thinking, AnthropicThinking::Disabled);

        Ok(AnthropicOptions {
            max_tokens: common.max_output_tokens,
            temperature: if thinking_enabled
                || !context.compat.supports_temperature
            {
                None
            } else {
                simple.temperature
            },
            thinking,
            thinking_display: patch
                .thinking_display
                .unwrap_or(AnthropicThinkingDisplay::Summarized),
            tool_choice: common.tool_choice,
            cache_retention: common.cache_retention,
            metadata_user_id: patch.metadata_user_id.clone(),
        })
    }
}
```

This carries forward Pi's adaptive-versus-budget distinction and its 1024-token answer reserve. Pi's wire builder also suppresses temperature during extended thinking and for models whose compat says temperature is unsupported. See `packages/ai/src/api/anthropic-messages.ts:1030–1135`.

> Correction: Pinned Pi's full Anthropic options distinguish an omitted `thinkingEnabled` from explicit `false`, retain the optional native tool-choice domain (`"auto"`, `"any"`, `"none"`, or a named tool), and carry the `interleavedThinking` request preference used during transport shaping. The Rust full-options shape therefore adds `AnthropicThinking::Omitted`, uses `tool_choice: Option<AnthropicToolChoice>`, and includes `interleaved_thinking: bool`; simple lowering still produces explicit disabled thinking when reasoning is absent and maps only the provider-neutral `auto`/`none` choices. This is necessary to preserve Pi's full-call wire omission, native tool choice, and beta-header behavior (`packages/ai/src/api/anthropic-messages.ts:168–277, 830–878, 880–970, 1030–1110`).

## 3.6 OpenAI-compatible Completions lowering

```rust
#[derive(Clone, Debug)]
pub struct OpenAiCompletionsOptions {
    pub max_tokens: Option<u32>,
    pub max_tokens_field: MaxTokensField,
    pub reasoning: OpenAiReasoningPlan,
    pub sampling: OrderedJsonObject,
    pub tool_choice: ToolChoice,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum OpenAiReasoningPlan {
    Disabled,

    ReasoningEffort {
        effort: String,
    },

    OpenRouter {
        effort: String,
    },

    DeepSeek {
        enabled: bool,
        effort: Option<String>,
    },

    ChatTemplate {
        kwargs: OrderedJsonObject,
    },

    StringThinking {
        value: String,
    },

    TokenBudget {
        field: ThinkingTokenBudgetField,
        budget: u32,
    },
}
```

> Correction: Pinned Pi's provider-neutral simple `ToolChoice` is only `"auto" | "none"`, but full `OpenAICompletionsOptions.toolChoice` is the optional native OpenAI SDK `ChatCompletionToolChoiceOption`. The Rust simple contract remains narrow; the full family options instead use `Option<OpenAiCompletionsToolChoice>` and represent `auto`, `none`, `required`, named function/custom choices, and `allowed_tools`. `None` retains omission, while simple lowering maps only an explicitly supplied provider-neutral choice into the native representation (`packages/ai/src/types.ts:82,310–318`; `packages/ai/src/api/openai-completions.ts:160–169,810–812`; OpenAI SDK 6.40.0 `ChatCompletionToolChoiceOption`).

> Correction: The single-variant reasoning plan above cannot represent pinned Pi's wire behavior. Pi applies thinking-format fields first and the top-level thinking-token-budget field independently, and Baseten emits `chat_template_args` plus mapped `reasoning_effort`. The Rust plan is therefore the product of a format-specific `OpenAiReasoningMode` and an optional `OpenAiReasoningTokenBudget`; Baseten's chat-template mode also retains an optional reasoning effort. The reasoning-effort mode retains whether its string came from the model's thinking-level map or from the requested level, because Ant Ling emits `reasoning.effort` only for a mapped string. `OpenAiCompletionsOptions` additionally retains named `temperature` separately from the ordered late `samplingParams` overlay, because a named temperature is inserted early while an overlay-only temperature is inserted late, and an overlay replacing a named temperature keeps the early property position (`packages/ai/src/api/openai-completions.ts:880–951`; `packages/ai/test/openai-completions-thinking-token-budget.test.ts`; `packages/ai/test/openai-completions-tool-choice.test.ts:1769–1840`; `packages/ai/test/baseten-models.test.ts`; `packages/ai/test/sampling-options.test.ts`).

> Correction: Pinned Pi preserves whether `thinkingBudgets` was omitted. That presence bit is observable for Google's Gemini 2.5 model-specific defaults: an omitted object selects Google's table, while an explicitly supplied object containing Pi's shared default values overrides that table. The Rust simple surface therefore uses `thinking_budgets: Option<ThinkingBudgets>`; non-Google lowerers substitute `ThinkingBudgets::default()` where the sketches below expect an always-present value (`packages/ai/src/api/google-generative-ai.ts:316–333,486–517`; `packages/ai/src/api/google-vertex.ts:331–348,568–597`; `packages/ai/src/api/simple-options.ts:57–70`).

```rust
impl ApiFamily for OpenAiCompletions {
    fn resolve_compat(
        effective_base_url: &Url,
        overrides: &OpenAiCompletionsCompat,
    ) -> Result<OpenAiCompletionsCompat, LoweringError> {
        let detected = detect_openai_compat(effective_base_url);
        Ok(detected.overlay(overrides))
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &OpenAiCompletionsSimplePatch,
    ) -> Result<OpenAiCompletionsOptions, LoweringError> {
        let common = plan_common(
            context.model.erased(),
            simple.context(),
            simple,
            simple.token_estimator(),
        )?;

        let reasoning_level = common
            .reasoning
            .map(|level| context.model.thinking_levels.resolve(level))
            .transpose()?;

        let reasoning = lower_openai_reasoning(
            reasoning_level,
            &context.compat,
            &simple.thinking_budgets,
            common.max_output_tokens,
        )?;

        let mut sampling = context.model.sampling_defaults.clone();
        sampling.overlay(&simple.sampling);
        sampling.overlay(&patch.sampling);

        Ok(OpenAiCompletionsOptions {
            max_tokens: Some(common.max_output_tokens),
            max_tokens_field: context.compat.max_tokens_field,
            reasoning,
            sampling,
            tool_choice: common.tool_choice,
            cache_retention: common.cache_retention,
            session_id: common.session_id,
        })
    }
}
```

Compatibility detection must use the **effective** base URL after authentication resolution. This matters for credentials such as Copilot or gateway credentials that supply a base URL.

> Correction: Pinned Pi's `detectCompat` also uses provider identity and, for OpenRouter, the model-id prefix for developer-role and Anthropic cache-control behavior. The adopted `ApiFamily::resolve_compat` signature intentionally receives only the effective URL plus typed overrides, so built-in provider catalog adapters must materialize every provider/model-dependent Pi value into `OpenAiCompletionsCompat`; URL-dependent values continue to be detected from the effective post-authentication URL. Custom descriptors using a provider-dependent Pi convention that is not inferable from their effective URL must supply the corresponding typed override.

## 3.7 `xhigh` and unsupported mappings

The typed model map must distinguish:

```rust
pub enum LevelSupport<T> {
    Unsupported,
    Disabled,
    Value(T),
}
```

An explicit model map entry of `null` in Pi means unsupported, while a missing key means use API defaults. See `packages/ai/src/types.ts:760–980`.

The Rust behavior should be:

```text
Explicit unsupported + strict request:
    LoweringError::UnsupportedReasoningLevel

Explicit unsupported + clamp policy:
    clamp to highest supported lower level, report diagnostic

Missing map:
    use API-family default mapping

Native xhigh support:
    pass through

API/model without native xhigh:
    clamp to high only when caller selected ReasoningFallback::Clamp
```

Pi often clamps `xhigh` or `max` to `high`; the Rust API should make strict-versus-clamp policy explicit. Pi-parity mode selects `Clamp`.

> Correction: Pinned Pi's model-aware clamp searches supported levels at and above the requested position first, then searches lower levels, and falls back to `off` when no supported mapping exists; it does not always choose the highest supported lower level. Rust Pi-parity mode preserves that order and fallback, including clamping an unsupported `xhigh` hole to a supported `max` (`packages/ai/src/models.ts:902–931`; `packages/ai/test/max-thinking.test.ts`).

---

# 4. Cross-provider handoff policy

## 4.1 Source correction

`AssistantMessage.diagnostics` exists in the message type, but the current `transformMessages` implementation does not append transformation diagnostics. It transforms and returns messages directly. See `packages/ai/src/types.ts:300–540` and `packages/ai/src/api/transform-messages.ts:1–380`.

The Copilot OpenAI-to-Anthropic behavior is also not a dedicated Copilot bridge in the current transform function. The test exercises the generic "different provider/API/model" path:

* thinking becomes text;
* tool-call thought signatures are removed;
* tool IDs are normalized;
* missing tool results are synthesized.

See `packages/ai/test/transform-messages-copilot-openai-to-anthropic.test.ts:1–360`.

## 4.2 Rust policy object

```rust
pub struct HandoffPolicy {
    pub loss_policy: HandoffLossPolicy,
    pub thinking_fallback: ThinkingFallback,
    pub image_fallback: ImageFallback,
    pub orphan_tool_policy: OrphanToolPolicy,
    pub failed_turn_policy: FailedTurnProjection,
}

pub enum HandoffLossPolicy {
    AllowAndReport,
    RejectLossy,
}

pub enum ThinkingFallback {
    /// Pi-compatible default.
    PlainText,

    /// Useful for APIs or applications that need explicit delineation.
    TaggedText {
        opening: String,
        closing: String,
    },

    Drop,
}

pub enum ImageFallback {
    PlaceholderText,
    Drop,
    Reject,
}

pub enum OrphanToolPolicy {
    SynthesizeErrorResult,
    DropCall,
    Reject,
}

pub enum FailedTurnProjection {
    Omit,
    IncludeDisplayTextOnly,
}
```

```rust
pub struct HandoffResult {
    pub context: Context,
    pub report: HandoffReport,
}

pub struct HandoffReport {
    pub source_models: BTreeSet<ModelFingerprint>,
    pub target: ModelFingerprint,
    pub changes: Vec<HandoffChange>,
    pub lossy: bool,
}

pub enum HandoffChange {
    FailedAssistantOmitted {
        message_id: MessageId,
        reason: AssistantFinishReason,
    },

    ImageReplaced {
        message_id: MessageId,
        block_id: ContentBlockId,
        placeholder: String,
    },

    OpaqueReplayDropped {
        message_id: MessageId,
        replay_item_id: ReplayItemId,
        kind: ReplayKind,
        reason: ReplayDropReason,
    },

    RedactedThinkingDropped {
        message_id: MessageId,
        block_id: ContentBlockId,
    },

    ThinkingConvertedToText {
        message_id: MessageId,
        block_id: ContentBlockId,
        tagged: bool,
    },

    ToolCallIdRewritten {
        message_id: MessageId,
        old: ToolCallId,
        new: ToolCallId,
    },

    ToolSignatureDropped {
        message_id: MessageId,
        tool_call_id: ToolCallId,
    },

    SyntheticToolResultInserted {
        tool_call_id: ToolCallId,
        tool_name: String,
    },

    EmptyBlockDropped {
        message_id: MessageId,
        block_id: ContentBlockId,
    },
}
```

## 4.3 Transformation order

Order matters because tool-result ID rewriting depends on the assistant rewrite, and orphan detection must happen after failed messages have been removed.

> Correction: Pinned Pi closes pending calls from a preceding successful assistant before it skips a later failed or aborted assistant. Rust therefore performs that boundary closure before failed-turn omission; it still never synthesizes results for calls contained in the omitted failed assistant itself (`packages/ai/src/api/transform-messages.ts:185–196`).

> Correction: Pinned Pi's first pass applies the target tool-call ID normalizer only to cross-model assistants and adds an old-ID-to-new-ID entry only when the normalized ID differs. Failed and aborted assistants still participate when they are cross-model, while same-model and no-op occurrences leave an earlier pass-wide mapping unchanged. A matching tool result is rewritten even after an intervening user or assistant message. The second pass then omits a failed assistant without treating its calls as pending or synthesizing results for them. Rust preserves those conditions and the pass-wide mapping before failed-turn omission (`packages/ai/src/api/transform-messages.ts:76–156,185–196`).

### Phase 1: structural normalization

```text
1. Normalize legacy null/undefined content to an empty block list.
2. Validate message IDs and content block IDs.
3. Normalize imported legacy replay fields into ReplayEnvelope.
```

Pi performs a null-content normalization for untyped callers and old sessions. See `packages/ai/src/api/transform-messages.ts:1–380`.

### Phase 2: remove terminally invalid assistant turns

```text
4. Omit assistant messages with finish Error or Aborted from the provider view.
5. Do not synthesize results for tool calls inside an omitted failed message.
```

The durable transcript is not changed.

### Phase 3: target capability downgrade

```text
6. Replace unsupported user images according to ImageFallback.
7. Replace unsupported tool-result images separately.
8. Collapse adjacent identical image placeholders.
```

Pi uses different user-image and tool-image placeholder text.

### Phase 4: replay applicability

For each assistant message:

```text
9. Compute source fingerprint:
       provider + api + produced-by model

10. Compare with target fingerprint.

11. Retain each replay item only if:
       item.applicability allows the target
       and the target API handler recognizes the item kind.

12. Report every discarded item.
```

No opaque data is silently passed through merely because it is valid JSON.

### Phase 5: content downgrade

For each assistant block:

```text
13. Redacted thinking:
       exact model and recognized replay payload → retain;
       otherwise → drop and report.

14. Signed or ordinary visible thinking:
       exact model → retain;
       cross-model → plain or tagged text according to policy.

15. Empty thinking:
       retain only if it has applicable replay data;
       otherwise drop.

16. Text:
       retain text;
       discard non-applicable text identity/signature metadata.

17. Tool call:
       discard non-applicable thought signatures and namespaces;
       retain semantic id/name/arguments for subsequent normalization.
```

Pi uses untagged text for its generic cross-model thinking conversion. `ThinkingFallback::PlainText` is therefore Pi parity. Tagged text is an explicit application policy, not a claim about current Pi.

### Phase 6: tool-call identity normalization

```text
18. Normalize tool-call IDs using target API rules.
19. Build old-id → new-id map.
20. Rewrite matching tool-result IDs using the same map.
21. Detect collisions after truncation/sanitization.
22. Resolve collisions deterministically with a stable hash.
```

A provider encoder can supply the normalizer:

```rust
pub trait ToolCallIdPolicy: Send + Sync {
    fn normalize(
        &self,
        original: &ToolCallId,
        source: &ModelFingerprint,
        target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError>;
}
```

### Phase 7: orphan closure

```text
23. For every retained successful assistant tool call, find a matching tool result
    before the next assistant or user interruption.

24. Under SynthesizeErrorResult, insert:
       role = tool-result
       is_error = true
       text = "No result provided"

25. Preserve assistant tool-call order when adding multiple synthetic results.
```

This matches Pi's current behavior.

### Phase 8: API-family final shaping

```text
26. Group consecutive tool results where the API requires one user message.
27. Insert role bridges only where the API compat requires them.
28. Drop blocks forbidden by the target API.
29. Validate that the final provider context is structurally accepted.
```

## 4.4 Surfacing losses

Losses should not be written into the historical `AssistantMessage.diagnostics`, because the same message may be projected differently for several target models.

Instead:

```rust
pub enum AgentEvent {
    ContextPrepared {
        turn: u32,
        target: ModelRef,
        report: HandoffReport,
    },
    // ...
}
```

The report is also available in tracing and telemetry.

Strict callers can reject any loss:

```rust
let prepared = context_policy.prepare(input).await?;

if prepared.report.lossy
    && matches!(policy.loss_policy, HandoffLossPolicy::RejectLossy)
{
    return Err(ContextError::LossyHandoff(prepared.report));
}
```

That is a deliberate improvement over Pi's current silent transformation.

---

# 5. Catalog data model, dynamic providers, overrides, and persistence

## 5.1 Avoiding an untyped metadata bag

Pi's `Model` includes:

* provider and API;
* base URL;
* reasoning support;
* `thinkingLevelMap`;
* input modalities;
* token pricing and request-wide tiers;
* context window and max output;
* sampling defaults;
* per-model headers;
* conditional, API-specific compat data.

See `packages/ai/src/types.ts:760–980`.

Lowering-critical data should not live in `BTreeMap<String, Value>`.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub common: CommonModelDescriptor,
    pub api: ApiModelConfig,

    /// Namespaced data not consumed by core lowering.
    pub extensions: ExtensionMap,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommonModelDescriptor {
    pub model_ref: ModelRef,
    pub display_name: String,
    pub base_url: Url,

    pub modalities: ModalityCapabilities,
    pub limits: ModelLimits,
    pub pricing: ModelPricing,

    pub reasoning: bool,
    pub headers: HeaderMapSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "api", content = "config")]
pub enum ApiModelConfig {
    OpenAiCompletions(OpenAiCompletionsModelConfig),
    OpenAiResponses(OpenAiResponsesModelConfig),
    AnthropicMessages(AnthropicMessagesModelConfig),
    GoogleGenerativeAi(GoogleModelConfig),
    GoogleVertex(GoogleModelConfig),
    BedrockConverse(BedrockModelConfig),
    MistralConversations(MistralModelConfig),

    Custom(CustomApiModelConfig),
}
```

### Typed OpenAI model configuration

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAiCompletionsModelConfig {
    pub compat: OpenAiCompletionsCompat,
    pub thinking_levels: ThinkingLevelMap<OpenAiThinkingValue>,
    pub sampling_defaults: OrderedJsonObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OpenAiThinkingValue {
    Disabled,
    Effort(String),
    TokenBudget(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAiCompletionsCompat {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    pub max_tokens_field: Option<MaxTokensField>,
    pub requires_tool_result_name: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub requires_thinking_as_text: Option<bool>,
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    pub thinking_format: Option<OpenAiThinkingFormat>,
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    pub supports_strict_mode: Option<bool>,
    pub supports_openai_grammar_tools: Option<bool>,
    pub cache_control_format: Option<CacheControlFormat>,
    pub session_affinity_format: Option<SessionAffinityFormat>,
    pub supports_long_cache_retention: Option<bool>,

    /// Forward-compatible fields only for the OpenAI-completions family.
    pub extensions: ExtensionMap,
}
```

Pi's OpenAI compat structure contains considerably more than a few boolean flags, including thinking formats, routing preferences, strict-tool support, cache behavior, and session affinity. See `packages/ai/src/types.ts:540–760`.

### Typed Anthropic model configuration

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnthropicMessagesModelConfig {
    pub compat: AnthropicMessagesCompat,
    pub thinking_levels: ThinkingLevelMap<AnthropicThinkingValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AnthropicThinkingValue {
    Off,
    Effort(AnthropicEffort),
    Budget(u32),
}
```

> Correction: Pinned Pi returns any string-valued `thinkingLevelMap` entry as an Anthropic effort, and the captured minimal-level corpus maps `minimal` to the provider string `"minimal"`. `AnthropicEffort` therefore includes `Minimal`; omitting it would make the captured request body diverge (`packages/ai/src/api/anthropic-messages.ts:805–814`; `providers/fixtures/anthropic-messages/reasoning-minimal`).

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnthropicMessagesCompat {
    pub supports_eager_tool_input_streaming: Option<bool>,
    pub supports_long_cache_retention: Option<bool>,
    pub send_session_affinity_headers: Option<bool>,
    pub supports_cache_control_on_tools: Option<bool>,
    pub supports_temperature: Option<bool>,
    pub force_adaptive_thinking: Option<bool>,
    pub allow_empty_signature: Option<bool>,
    pub supports_strict_tools: Option<bool>,
    pub supports_tool_references: Option<bool>,
    pub allowed_fallback_models: Vec<AnthropicFallbackModel>,

    pub extensions: ExtensionMap,
}
```

### Typed level map

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThinkingLevelMap<T> {
    pub off: Option<LevelSupport<T>>,
    pub minimal: Option<LevelSupport<T>>,
    pub low: Option<LevelSupport<T>>,
    pub medium: Option<LevelSupport<T>>,
    pub high: Option<LevelSupport<T>>,
    pub xhigh: Option<LevelSupport<T>>,
    pub max: Option<LevelSupport<T>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LevelSupport<T> {
    Unsupported,
    Value(T),
}
```

The outer `Option` distinguishes "catalog has no mapping; use API default" from explicit unsupported.

### Open extensions

```rust
pub type ExtensionMap = BTreeMap<ExtensionId, VersionedExtension>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionedExtension {
    pub schema_version: u32,
    pub value: Box<serde_json::value::RawValue>,
}
```

Rules:

* API-family lowering may read only its typed config.
* Provider-specific middleware may read a declared namespaced extension.
* Unknown extensions survive persistence.
* Core behavior cannot depend on an ad hoc string key in `metadata`.

## 5.2 Pricing shape

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPricing {
    pub default: TokenPriceRates,
    pub request_wide_tiers: Vec<RequestWidePriceTier>,
    pub cache_write_retention: CacheWriteRetentionPricing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenPriceRates {
    /// Integer micro-dollars per million tokens.
    pub input: MoneyRate,
    pub output: MoneyRate,
    pub cache_read: MoneyRate,
    pub cache_write: MoneyRate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestWidePriceTier {
    pub input_tokens_above: u64,
    pub rates: TokenPriceRates,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheWriteRetentionPricing {
    pub short: Option<MoneyRate>,
    pub one_hour: Option<MoneyRate>,
}
```

This avoids `f64` monetary arithmetic and can represent cache-retention-specific rates without burying them in extensions.

> Correction: Pinned Pi retains the published `-1000000` OpenRouter rates and `calculateCost` applies them directly. Rust therefore uses signed fixed-point integers for `MoneyRate` and `Cost.micros`, retaining and calculating those negative values without `f64` arithmetic (`packages/ai/src/models.ts:878–897`; published `openrouter/auto` and `openrouter/auto-beta` catalog records).

> Correction: Pinned Pi retains Anthropic's provider-reported one-hour cache-creation subset as `usage.cacheWrite1h` and prices only that subset at twice the input rate; remaining cache-write tokens use the ordinary published cache-write rate, and an absent breakdown means a zero one-hour subset. Rust `Usage` therefore includes `cache_write_one_hour_tokens: Option<u64>`, Anthropic decoding reads `cache_creation.ephemeral_1h_input_tokens`, and checked integer pricing separates the mixed-retention portions (`packages/ai/src/api/anthropic-messages.ts:600–615`; `packages/ai/src/models.ts:878–897`; `packages/ai/test/anthropic-cache-write-1h-cost.test.ts`).

## 5.3 Catalog layers

Do not persist one flattened effective catalog. Preserve provenance:

```rust
pub struct ProviderCatalogLayers {
    /// Generated or provider-factory baseline.
    pub baseline: Arc<[ModelDescriptor]>,

    /// Last durable provider-owned dynamic snapshot.
    pub restored_dynamic: Option<Arc<CatalogSnapshot>>,

    /// Most recent network candidate, after validation.
    pub network_dynamic: Option<Arc<CatalogSnapshot>>,

    /// Host-managed persistent overrides, such as models.json.
    pub host_overrides: Arc<[ModelOverride]>,

    /// Process-local temporary registrations.
    pub runtime_overrides: Arc<[ModelOverride]>,
}
```

Composition order:

```text
baseline
    < restored dynamic snapshot
    < latest network snapshot
    < persistent host overrides
    < process-local runtime overrides
```

An override can:

* add a model;
* hide a model;
* replace base URL;
* override common limits or pricing;
* apply typed API-family compat changes;
* add per-model headers.

An override may not change `api` while retaining incompatible typed config. That is a catalog validation error.

## 5.4 Catalog source and store traits

```rust
pub trait ModelCatalogSource: Send + Sync {
    fn baseline(&self) -> Arc<[ModelDescriptor]>;

    fn fetch(
        &self,
        context: CatalogFetchContext,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<CatalogCandidate, CatalogError>>;
}

pub struct CatalogCandidate {
    pub models: Vec<ModelDescriptor>,
    pub checked_at: Timestamp,
    pub revision: Option<String>,
    pub etag: Option<String>,
    pub source_metadata: ExtensionMap,
}

pub trait ModelsStore: Send + Sync {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>>;

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<(), StoreError>>;

    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<(), StoreError>>;
}

pub trait ModelOverrideStore: Send + Sync {
    fn snapshot(
        &self,
        provider: &ProviderId,
    ) -> Result<Arc<[ModelOverride]>, OverrideError>;
}
```

`ModelsStore` contains provider-originated dynamic data. `ModelOverrideStore` contains host policy. They are intentionally separate.

## 5.5 Restore, refresh, persist, and publish

Pi's `Models` implementation:

* tracks per-provider refresh generations;
* cancels superseded refreshes;
* serializes publication;
* reads stored state first;
* runs a no-network restore phase;
* resolves credentials;
* runs a network phase;
* persists a publication before applying its in-memory update;
* refreshes providers concurrently and returns per-provider errors.

See `packages/ai/src/models.ts:250–760`.

The Rust process should be:

```text
Provider registration
    ↓
Read persisted provider snapshot
    ↓
Validate persisted snapshot
    ↓
Compose:
    baseline + persisted snapshot + host overrides + runtime overrides
    ↓
Publish one immutable effective snapshot
    ↓
Resolve refresh credential
    ↓
Fetch network candidate
    ↓
Validate the complete candidate
    ↓
Persist provider-owned candidate
    ↓
Re-check generation/cancellation
    ↓
Compose:
    baseline + network candidate + host overrides + runtime overrides
    ↓
Atomically publish one immutable effective snapshot
```

Persist-before-publish should remain the default. It means a failed durable write does not temporarily expose a catalog that will disappear after restart.

```rust
async fn publish_candidate(
    state: &ProviderCatalogState,
    generation: RefreshGeneration,
    candidate: CatalogCandidate,
    store: &dyn ModelsStore,
    overrides: &dyn ModelOverrideStore,
    cancel: CancellationToken,
) -> Result<bool, CatalogError> {
    let validated = validate_candidate(candidate)?;

    state.verify_generation(generation, &cancel)?;

    store
        .write(&state.provider_id, &validated.to_persisted(), cancel.clone())
        .await?;

    state.verify_generation(generation, &cancel)?;

    let effective = compose_effective_catalog(
        state.baseline(),
        Some(validated),
        overrides.snapshot(&state.provider_id)?,
        state.runtime_overrides(),
    )?;

    state.publish(Arc::new(effective));
    Ok(true)
}
```

The publication operation must not hold a read/write lock across network I/O. Readers load an `Arc<CatalogSnapshot>` atomically.

## 5.6 Host override updates

When `models.json` or another host override changes:

```text
1. Parse and type-check the override file.
2. Retain the last valid override snapshot if parsing fails.
3. Recompose every affected provider from its last provider-owned snapshot.
4. Atomically publish the new effective snapshot.
5. Do not write the flattened result to ModelsStore.
```

This avoids a removed override becoming permanently baked into the dynamic-provider cache.

## 5.7 Static and dynamic provider semantics

Synchronous reads remain against the current immutable snapshot. Refresh is explicit and asynchronous. Static providers make refresh a no-op. That matches Pi's documented provider collection behavior.

> Correction: Pinned Pi implements the static-provider no-op by filtering static and unknown providers out of the refresh work and result entirely. It also suppresses a provider error whenever that provider's composed signal is aborted, including a refresh superseded by a newer generation (`packages/ai/src/models.ts:306–430`; `packages/ai/test/models-runtime.test.ts:515–614`). The Rust `Models::refresh` report follows that observable behavior: only selected dynamic, non-aborted provider generations receive per-provider entries.

```rust
pub struct RefreshReport {
    pub aborted: bool,
    pub providers: BTreeMap<ProviderId, ProviderRefreshResult>,
}

pub enum ProviderRefreshResult {
    NotRefreshable,
    RestoredOnly {
        model_count: usize,
    },
    Refreshed {
        old_revision: Option<String>,
        new_revision: Option<String>,
        model_count: usize,
    },
    Failed {
        restored_model_count: usize,
        error: CatalogErrorReport,
    },
}
```

A dynamic Radius-style provider is simply a `ModelCatalogSource` whose `fetch` uses credential-derived gateway configuration.

---

# 6. Authentication interaction, loopback flows, and FFI

## 6.1 What Pi's interaction contract currently covers

Pi's `AuthInteraction` supports:

* text prompts;
* secret prompts;
* select prompts;
* manual-code prompts;
* informational events;
* auth-URL events;
* device-code events;
* progress events;
* whole-flow cancellation;
* per-prompt cancellation, allowing a callback to cancel a losing manual-code prompt.

OAuth credentials are open objects with canonical `access`, `refresh`, and `expires` fields plus provider-specific fields. See `packages/ai/src/auth/types.ts:1–340`.

The shared device-code implementation follows RFC 8628 timing rules, including a five-second default interval, a one-second minimum, server-driven or five-second `slow_down` increments, deadline handling, and cancellation. See `packages/ai/src/auth/oauth/device-code.ts:1–240`.

The OpenAI Codex flow demonstrates the difficult case:

* PKCE and state generation;
* a local callback server;
* success/error HTML;
* a manual code or redirect-URL prompt;
* a race between callback and manual input;
* cancellation of the loser;
* device-code alternative;
* extraction of provider-specific account data.

See `packages/ai/src/auth/oauth/openai-codex.ts:120–520`.

> Correction: Pinned Codex request execution also accepts a caller-supplied OAuth access token through the request `apiKey` option, derives `chatgpt_account_id` directly from that JWT, and does not require a stored refresh credential for that path. The Codex provider therefore registers a non-ambient direct-token `ApiKeyAuth` method alongside its OAuth method for both Send and Local resolvers; stored OAuth remains the provider-owned persistent credential (`packages/ai/src/api/openai-codex-responses.ts:252–283`; `packages/ai/src/auth/oauth/openai-codex.ts:97–112`).

> Correction: Pinned Codex browser OAuth always advertises and exchanges the registered `http://localhost:1455/auth/callback` URI, even when a host-owned fixed-loopback receiver binds and reports the equivalent numeric `127.0.0.1` address. The receiver callback must include the exact generated state, while manually pasted raw authorization codes retain Pi's optional-state behavior and pasted redirect URLs validate state when present. Rust keeps callback reception host-owned but applies those provider validation and registered-URI rules in both Send and Local flows (`packages/ai/src/auth/oauth/openai-codex.ts:73–96,295–352,421–500`).

## 6.2 Native Rust host contract

```rust
pub trait AuthInteraction: Send + Sync {
    fn capabilities(&self) -> AuthHostCapabilities;

    fn prompt(
        &self,
        prompt: AuthPrompt,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AuthAnswer, AuthInteractionError>>;

    fn notify(
        &self,
        event: AuthEvent,
    ) -> Result<(), AuthInteractionError>;

    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>>;
}
```

```rust
#[derive(Clone, Debug)]
pub struct AuthHostCapabilities {
    pub external_browser: bool,
    pub loopback_http: bool,
    pub custom_url_scheme: bool,
    pub universal_links: bool,
    pub manual_paste: bool,
    pub clipboard: bool,
}

#[derive(Clone, Debug)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthSelectOption>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
        challenge_id: AuthChallengeId,
    },
}

#[derive(Clone, Debug)]
pub enum AuthAnswer {
    Text(String),
    Selected(String),
}
```

```rust
#[derive(Clone, Debug)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
    },

    OpenUrl {
        challenge_id: AuthChallengeId,
        url: Url,
        instructions: Option<String>,
    },

    DeviceCode {
        challenge_id: AuthChallengeId,
        user_code: String,
        verification_uri: Url,
        interval: Option<Duration>,
        expires_in: Option<Duration>,
    },

    Progress {
        message: String,
    },
}
```

## 6.3 Redirect receiver abstraction

```rust
pub struct RedirectReceiverRequest {
    pub challenge_id: AuthChallengeId,
    pub preferred: Vec<RedirectStrategy>,
    pub expected_path: Option<String>,
    pub success_page: AuthHtmlPage,
    pub failure_page: AuthHtmlPage,
}

pub enum RedirectStrategy {
    FixedLoopback {
        host: IpAddr,
        port: u16,
        path: String,
    },

    EphemeralLoopback {
        host: IpAddr,
        path: String,
    },

    CustomScheme {
        scheme: String,
        path: String,
    },

    UniversalLink {
        origin: Url,
        path: String,
    },

    ManualPaste,
}

pub trait RedirectReceiver: Send {
    fn redirect_uri(&self) -> &Url;

    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'static, Result<RedirectArrival, AuthInteractionError>>;
}

pub struct RedirectArrival {
    pub url: Url,
    pub received_at: Timestamp,
}
```

### Responsibility split

**`pi-ai` owns:**

* PKCE verifier/challenge generation;
* OAuth state generation and validation;
* authorization URL construction;
* token exchange and refresh;
* device-code polling;
* interval, deadline, and backoff rules;
* parsing a pasted code or redirect URL;
* racing valid completion paths;
* provider-specific credential normalization;
* expiry calculation;
* provider-specific extra credential fields.

**The host owns:**

* displaying prompts and progress;
* opening the system browser;
* running a loopback server when supported;
* registering and receiving custom URL schemes;
* receiving universal links;
* rendering browser callback result pages;
* clipboard integration;
* delivering callback URLs to the flow;
* platform lifecycle integration.

A mobile application should not embed a loopback HTTP server in `pi-ai`. Its host adapter returns a custom-scheme or universal-link receiver where the provider supports one.

If a provider requires a fixed localhost redirect URI and offers no device or manual alternative, the flow should fail with:

```rust
AuthError::UnsupportedRedirectStrategy {
    provider: ProviderId,
    required: RedirectStrategyDescription,
    host_capabilities: AuthHostCapabilities,
}
```

It should not silently construct a loopback URI that the host cannot receive.

## 6.4 Racing callback and manual input

Native Rust can use two host operations:

```rust
let redirect = receiver.receive(cancel.child());
let manual = interaction.prompt(
    AuthPrompt::ManualCode { ... },
    cancel.child(),
);

let winner = select_first_valid(redirect, manual, cancel.clone()).await?;
```

The losing child token is cancelled after a valid result is accepted. Invalid manual input does not necessarily defeat a still-pending callback; the flow policy can continue waiting or report validation and prompt again.

## 6.5 FFI shape

Passing Rust async trait callbacks directly across a general C ABI is undesirable. The FFI layer should expose an explicit auth session state machine:

```c
pi_auth_session *pi_auth_login_begin(
    pi_models *,
    const char *provider_id,
    const char *auth_type,
    const char *host_capabilities_json
);

pi_status pi_auth_session_next(
    pi_auth_session *,
    pi_auth_challenge *out_challenge
);

pi_status pi_auth_session_respond(
    pi_auth_session *,
    const char *challenge_id,
    const char *response_json
);

void pi_auth_session_cancel(pi_auth_session *);
void pi_auth_session_destroy(pi_auth_session *);
```

Representative challenge:

```json
{
  "id": "challenge-7",
  "type": "open_url",
  "url": "https://...",
  "redirect": {
    "strategy": "custom_scheme",
    "uri": "myapp://oauth/callback"
  },
  "alsoAcceptsManualCode": true
}
```

The host can later supply either:

```json
{
  "type": "redirect_arrived",
  "url": "myapp://oauth/callback?code=...&state=..."
}
```

or:

```json
{
  "type": "manual_code",
  "value": "..."
}
```

The first valid response wins. A response to a closed challenge returns `PI_AUTH_CHALLENGE_SUPERSEDED`.

Device-code polling remains inside Rust. The host only receives the code and progress state.

## 6.6 Typed provider-specific credential extras

Do not expose all OAuth extras as an unstructured map internally.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: Timestamp,
    pub extra: ProviderOAuthExtra,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", content = "value")]
pub enum ProviderOAuthExtra {
    None,

    Radius {
        gateway_url: Url,
        organization_id: Option<String>,
    },

    GitHubCopilot {
        api_endpoint: Url,
        account_id: Option<String>,
    },

    OpenAiCodex {
        account_id: String,
    },

    Custom {
        schema: ExtensionId,
        schema_version: u32,
        value: Box<serde_json::value::RawValue>,
    },
}
```

> Correction: Pinned Pi's GitHub Copilot OAuth result also carries
> `availableModelIds`, and `providers/github-copilot.ts` uses that
> entitlement-derived list to filter the credential-visible catalog. The Rust
> `GitHubCopilot` variant therefore also carries
> `available_model_ids: Option<Vec<ModelId>>`; omitting it would make
> credential-scoped availability lossy.

> Correction: Pinned Pi persists GitHub Copilot's normalized enterprise domain
> as a distinct `enterpriseUrl` field and uses it during both refresh and
> request-auth conversion. It is not account identity. The Rust
> `GitHubCopilot` variant therefore also carries
> `enterprise_url: Option<String>` independently of `account_id`, and the
> credential writer preserves it (`packages/ai/src/auth/oauth/github-copilot.ts:341–347,487–505`).

At the persistence boundary, unknown provider extras still round-trip through `Custom`.

---

# 7. Harness architecture

## 7.1 Source status

The current Pi harness directory contains substantial implemented subsystems, but the top-level `AgentHarness` itself remains largely a scaffold whose main orchestration operations reject with `HarnessNotImplemented`. The Rust design should derive behavior from the implemented session, reducer, compaction, environment, tool, and telemetry contracts rather than pretending the top-level class already defines completed orchestration semantics. See `packages/agent/src/harness/agent-harness.ts:300–560`.

## 7.2 Session protocol

Pi separates:

* immutable branch-tree `Entry` records;
* lane-scoped operational `Record` values;
* lane head movements;
* global facts such as names and labels;
* one shared monotonically increasing sequence.

Its entries include messages, model changes, thinking-level changes, active-tool changes, compactions, branch summaries, and custom data. Operational records include operation starts/finishes, abort requests, step attempts, tool starts, queue changes, deferred writes, and usage. See `packages/agent/src/harness/session/types.ts:1–360`.

The Rust data model should preserve that separation.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub parent_session_id: Option<SessionId>,
    pub environment: SessionEnvironmentMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntryBase {
    pub id: EntryId,
    pub sequence: Sequence,
    pub parent_id: Option<EntryId>,
    pub timestamp: Timestamp,
}
```

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionEntry {
    Message {
        base: EntryBase,
        message: AgentRecord,
        terminate: bool,
    },

    ModelChange {
        base: EntryBase,
        model: ModelRef,
    },

    ReasoningChange {
        base: EntryBase,
        level: ReasoningLevel,
    },

    ActiveToolsChange {
        base: EntryBase,
        tool_names: Vec<String>,
    },

    Compaction {
        base: EntryBase,
        summary: String,
        retained_tail: Vec<AgentRecord>,
        tokens_before: u64,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
    },

    BranchSummary {
        base: EntryBase,
        from_id: EntryId,
        summary: String,
        details: Option<VersionedExtension>,
        usage: Option<Usage>,
    },

    Custom {
        base: EntryBase,
        custom_type: String,
        data: Option<VersionedExtension>,
    },
}
```

Operational records are not branch entries:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OperationRecord {
    Started {
        base: OperationRecordBase,
        source_leaf_id: Option<EntryId>,
        intent: OperationIntent,
    },

    AbortRequested {
        base: OperationRecordBase,
        run_id: RunId,
    },

    Finished {
        base: OperationRecordBase,
        run_id: RunId,
        outcome: OperationOutcome,
        error: Option<PublicError>,
    },

    StepAttempt {
        base: OperationRecordBase,
        run_id: RunId,
        step: OperationStep,
        attempt: u32,
        result_entry_id: EntryId,
        compaction_reason: Option<CompactionReason>,
    },

    ToolStarted {
        base: OperationRecordBase,
        run_id: RunId,
        assistant_entry_id: EntryId,
        tool_index: u32,
        call: ToolCallIdentity,
        effective_args: serde_json::Value,
        result_entry_id: EntryId,
        replay: ToolReplayPolicy,
    },

    QueueEnqueued {
        base: OperationRecordBase,
        run_id: Option<RunId>,
        queue: QueueKind,
        target: ProvisionedEntry,
    },

    QueueCancelled {
        base: OperationRecordBase,
        run_id: Option<RunId>,
        entry_id: EntryId,
    },

    WriteDeferred {
        base: OperationRecordBase,
        run_id: RunId,
        target: ProvisionedEntry,
    },

    Usage {
        base: OperationRecordBase,
        attribution: UsageAttribution,
        usage: Usage,
    },
}
```

## 7.3 Storage traits

```rust
pub trait SessionStorage: Send + Sync {
    fn metadata(
        &self,
    ) -> BoxFuture<'_, Result<SessionMetadata, SessionError>>;

    fn load_state(
        &self,
    ) -> BoxFuture<'_, Result<SessionState, SessionError>>;

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> BoxFuture<'_, Result<AppendReceipt, SessionError>>;

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> BoxFuture<'_, Result<Vec<SessionMutation>, SessionError>>;

    fn repair_tail(
        &self,
    ) -> BoxFuture<'_, Result<TailRepairReport, SessionError>>;
}
```

```rust
pub trait SessionRepository: Send + Sync {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    fn open(
        &self,
        id: &SessionId,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    fn list(
        &self,
        query: SessionQuery,
    ) -> BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>>;
}
```

An atomic `append` batch is a native improvement over individually appending closely related records. The Pi-v4 adapter can serialize the batch as consecutive lines under one append lock.

## 7.4 Reducer

All current state should be derivable from the append log:

```rust
pub trait SessionReducer {
    fn apply(
        &mut self,
        mutation: &SessionMutation,
    ) -> Result<(), SessionReductionError>;

    fn state(&self) -> &SessionState;
}
```

Core invariants:

```text
1. Global sequence is consecutive.
2. Entry parent IDs refer to earlier entries.
3. A lane points to zero or one existing entry.
4. Operation-finished refers to one earlier open operation.
5. At most one open operation exists per lane.
6. A tool-start record refers to the assistant entry and stable tool index.
7. Queue cancellation refers to a provisioned queued entry.
8. A durable operation can be resumed or explicitly abandoned after recovery.
```

Snapshots may accelerate loading, but the mutation log remains authoritative.

## 7.5 Branching

A branch is not a copied transcript. It is a lane head pointing into an immutable entry tree.

```rust
pub struct LaneState {
    pub name: LaneName,
    pub leaf_id: Option<EntryId>,
}

pub enum ForkPosition {
    Before(EntryId),
    At(EntryId),
    WholeTree,
}
```

Appending an entry:

```text
new_entry.parent_id = current lane leaf
current lane leaf = new_entry.id
```

Moving a lane changes only the pointer.

Branch navigation with summarization:

```text
1. Determine current branch path and target branch path.
2. Find their common ancestor.
3. Optionally summarize the abandoned segment.
4. Append BranchSummaryEntry under the target continuation point.
5. Move or create the lane.
6. Persist operation outcome.
```

No existing entry is mutated.

## 7.6 Operation recovery

The operational log makes crashes observable:

```rust
pub enum RecoveryDecision {
    Idle,

    Resume {
        operation: OperationRecord,
        completed_steps: Vec<OperationRecord>,
    },

    Abandon {
        operation: OperationRecord,
        reason: PublicError,
    },

    Corrupt {
        open_operations: Vec<OperationRecord>,
    },
}
```

A process opening a session examines open operations for the selected lane. More than one unresolved operation is corruption, matching the intent documented by Pi's session contract.

## 7.7 Compaction as a context policy

Compaction should not be embedded in `Agent`. It is a harness policy layered over `ContextPolicy`.

```rust
pub trait CompactionPolicy: Send + Sync {
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError>;

    fn compact(
        &self,
        input: CompactionInput,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<CompactionResult, CompactionError>>;
}

pub enum CompactionDecision {
    NoCompaction,

    Compact {
        reason: CompactionReason,
        retained_tail_start: usize,
        summary_model: ModelRef,
    },
}
```

```rust
pub struct HarnessContextPolicy {
    pub base: Arc<dyn ContextPolicy>,
    pub compaction: Arc<dyn CompactionPolicy>,
    pub session: Arc<Session>,
}
```

Preparation flow:

```text
1. Reconstruct branch messages.
2. Apply prior compaction entries.
3. Estimate current context.
4. Ask CompactionPolicy.
5. If no compaction, delegate to base ContextPolicy.
6. If compaction:
      a. append operation_started;
      b. append step_attempt;
      c. call summary ModelRuntime;
      d. append usage;
      e. append CompactionEntry with summary and retained tail;
      f. append operation_finished;
      g. reconstruct context from the new branch leaf.
7. Delegate final handoff projection to base ContextPolicy.
```

A failed compaction must not advance the branch head to a nonexistent or partial summary entry.

### Overflow retry

Context overflow is a separate trigger:

```text
assistant attempt fails with classified context overflow
    ↓
record failed attempt
    ↓
compact with reason = overflow
    ↓
retry assistant step under the same harness operation
```

This is a harness retry, not the provider transport retry from §2.

## 7.8 Branch summarization

```rust
pub trait BranchSummaryPolicy: Send + Sync {
    fn summarize(
        &self,
        input: BranchSummaryInput,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>>;
}
```

The summary input should include:

* common ancestor;
* abandoned branch entries;
* target branch tail;
* custom navigation instructions;
* active model/tool state;
* token budget.

The result is a durable `BranchSummaryEntry`, not an ephemeral system-prompt string.

## 7.9 Skills and prompt templates

```rust
pub trait SkillCatalog: Send + Sync {
    fn list(
        &self,
    ) -> BoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>>;

    fn load(
        &self,
        id: &SkillId,
    ) -> BoxFuture<'_, Result<LoadedSkill, SkillError>>;
}

pub struct LoadedSkill {
    pub descriptor: SkillDescriptor,
    pub prompt_fragments: Vec<PromptFragment>,
    pub resources: Vec<SkillResource>,
    pub digest: ContentDigest,
}
```

```rust
pub trait PromptTemplateRegistry: Send + Sync {
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError>;
}
```

The harness operation intent should record skill/template identities and content digests. A resumed operation must not silently pick up changed skill content unless the resume policy explicitly allows it.

## 7.10 Environment contract

Pi's environment layer is capability-oriented and its Node implementation handles process-tree termination and a grace period for trailing stdio. The agent core should not expose Node/Tokio process types. See `packages/agent/src/harness/env/nodejs.ts` and the harness environment types.

```rust
pub trait AgentEnvironment: Send + Sync {
    fn filesystem(&self) -> &dyn AgentFileSystem;
    fn processes(&self) -> &dyn ProcessSpawner;
    fn clock(&self) -> &dyn Clock;
    fn temporary_artifacts(&self) -> &dyn TemporaryArtifactStore;
}
```

```rust
pub trait AgentFileSystem: Send + Sync {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> BoxFuture<'_, Result<CanonicalPath, FileSystemError>>;

    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<FileReadResult, FileSystemError>>;

    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<FileWriteResult, FileSystemError>>;

    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<EditResult, FileSystemError>>;
}
```

```rust
pub trait ProcessSpawner: Send + Sync {
    fn spawn(
        &self,
        command: ProcessCommand,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, ProcessError>>;
}

pub trait RunningProcess: Send {
    fn events(&mut self) -> ProcessEventStream<'_>;

    fn terminate(
        &mut self,
        policy: TerminationPolicy,
    ) -> BoxFuture<'_, Result<ProcessOutcome, ProcessError>>;
}
```

```rust
pub struct TerminationPolicy {
    pub graceful_signal: TerminationSignal,
    pub graceful_timeout: Duration,
    pub forced_timeout: Duration,
    pub stdio_grace_period: Duration,
    pub terminate_process_tree: bool,
}
```

On mobile or WASM, `ProcessSpawner` may return `CapabilityUnavailable`; tools can decide whether that is a tool error or removes the tool from the active set.

## 7.11 Reference tools

The reference tools should live in `pi-agent-harness`, not `pi-agent-core`.

### File mutation queue

Pi serializes writes and edits per canonical path. The Rust equivalent:

```rust
pub struct FileMutationQueue {
    locks: DashMap<CanonicalPath, Arc<AsyncMutex<()>>>,
}

impl FileMutationQueue {
    pub async fn with_path_lock<T>(
        &self,
        path: CanonicalPath,
        operation: impl Future<Output = T>,
    ) -> T;
}
```

This prevents concurrent assistant tool calls from racing on the same file while allowing unrelated files to proceed concurrently. See the Pi file-mutation queue source.

### Edit semantics

`edit` should:

1. read under the path lock;
2. require the target text to occur exactly once unless an occurrence is explicitly selected;
3. reject a no-op replacement;
4. write atomically where supported;
5. return a structured diff and resulting metadata.

### Output truncation

Truncation should track both:

* UTF-8 bytes;
* logical lines.

```rust
pub struct TruncationLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub strategy: TruncationStrategy,
}

pub enum TruncationStrategy {
    Head,
    Tail,
    HeadAndTail,
}
```

It must not split a UTF-8 sequence. Pi's truncation and reference tools make these behaviors observable.

### Bash full-output recovery

When shell output is truncated:

```rust
pub struct BashToolResultDetails {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub truncated: bool,
    pub total_bytes: u64,
    pub total_lines: u64,
    pub full_output_artifact: Option<ArtifactRef>,
}
```

The displayed tool result can be bounded while retaining a reference to complete output.

## 7.12 Telemetry

Telemetry should have a published versioned envelope distinct from `AgentEvent` and session records.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    pub schema_version: u32,
    pub event_id: TelemetryEventId,
    pub timestamp: Timestamp,

    pub session_id: Option<SessionId>,
    pub lane: Option<LaneName>,
    pub run_id: Option<RunId>,
    pub operation_id: Option<OperationId>,
    pub sequence: Option<Sequence>,

    pub event: TelemetryEvent,
}
```

```rust
pub enum TelemetryEvent {
    RunStarted { model: ModelRef },
    ModelRequestStarted { attempt: u32 },
    ModelRequestFinished {
        finish: AssistantFinishReason,
        usage: Usage,
        duration: Duration,
    },
    ToolStarted { tool_name: String },
    ToolFinished {
        tool_name: String,
        success: bool,
        duration: Duration,
    },
    CompactionStarted { reason: CompactionReason },
    CompactionFinished { usage: Usage },
    SessionMutationCommitted { mutation_kind: String },
    HandoffPerformed { report: HandoffTelemetrySummary },
}
```

```rust
pub trait TelemetrySink: Send + Sync {
    fn emit(
        &self,
        event: TelemetryEnvelope,
    ) -> BoxFuture<'_, Result<(), TelemetryError>>;
}
```

Defaults:

* prompt, response, tool arguments, tool output, auth data, headers, and replay payloads are excluded;
* sink failure does not fail a model run unless a compliance policy explicitly requires it;
* durable mutation telemetry is emitted only after storage acceptance;
* schema changes require a new `schema_version`;
* a JSON Schema artifact is generated from the Rust type and checked into the repository.

## 7.13 Should Rust read and write Pi's existing JSONL?

### Recommendation

**Read Pi v4 JSONL: yes.**

**Write Pi v4 JSONL: yes, but only through an explicit constrained compatibility backend.**

**Use a new native format by default.**

### Why reading matters

It provides:

* migration of existing sessions;
* continuation of branches and compactions;
* verification of the Rust reducer against real Pi logs;
* a direct parity corpus;
* lower adoption friction.

Pi v4 uses a strict header plus append-only mutations for entries, records, lane changes, and facts. See `packages/agent/src/harness/session/jsonl/codec.ts:1–320`.

Its storage also relies on sequence validation, torn-tail recovery, serialized append, and atomic rewrite behavior. Those are protocol semantics, not just JSONL syntax.

### Why Pi v4 should not be the native format

Pi v4 cannot cleanly represent every proposed native feature:

* message-level ordered replay envelopes;
* explicit replay completeness;
* structured handoff reports;
* native batch commit boundaries;
* checksums;
* richer provider error structures;
* typed extension schemas;
* future operation state.

Encoding those into arbitrary `custom` records would make Pi unaware of semantics that affect replay correctness.

### Compatibility writer rules

`PiV4SessionStorage` should:

* append only records representable in Pi v4;
* reject an unrepresentable mutation instead of dropping fields;
* preserve all existing file bytes during append;
* use Pi-compatible field names and JSON serialization for appended lines;
* retain unknown fields from imported lines in a raw sidecar representation;
* support branch/tree rewrite only when every record is representable.

A fork or full rewrite may canonicalize JSON and therefore need not be byte-identical to the original file. Existing append-only bytes should remain untouched.

Provider request bodies, by contrast, should have strict byte parity. Session compatibility is semantic plus append preservation; it should not freeze the native harness protocol indefinitely.

---

# 8. Mapping Pi agent-core's public contract

## 8.1 Surface mapping

| Pi public surface | Actual Pi phase and behavior | Rust mapping |
| --- | --- | --- |
| `steer(message)` | Enqueues a steering message. Initial steering is polled at loop start. Later steering is polled after an entire turn: assistant response, all tools, `turn_end`, `prepareNextTurn`, and `shouldStopAfterTurn`. It is **not polled after each individual tool**. | `AgentControl::steer`. Receiver is polled at `InitialQueuePoll` and `AfterTurnPolicy`, never during a tool batch. |
| `followUp(message)` | Polled only when there are no more tool calls and no pending steering messages—when the agent would otherwise stop. | `AgentControl::follow_up`, polled at `WouldStop`. |
| queue mode `"one-at-a-time"` / `"all"` | Drain first queued item or all queued items. | `QueueDrainMode::{One, All}` stored independently for steering and follow-up. |
| `transformContext` | Runs before each LLM call on `AgentMessage[]`. | `ContextPolicy::prepare_agent_records`. |
| `convertToLlm` | Runs after `transformContext`, converting custom agent messages to provider-neutral LLM messages. | `MessageProjector::project`. |
| `beforeToolCall` | After tool lookup, argument preparation, and validation; before execution. Parallel mode still performs this preflight sequentially. | `ToolPolicy::authorize`, called during deterministic preflight. |
| `afterToolCall` | After execution, before `tool_execution_end` and tool-result message emission. Can modify result/error/termination. | `ToolPolicy::finalize`. |
| `prepareNextTurn` | After `turn_end`; may replace context, model, and reasoning level. | `TurnPolicy::prepare_next_turn`. |
| `prepareNextTurnWithContext` | Same phase, with full completed-turn context. | The only native form; `TurnPolicy` always receives `CompletedTurn`. |
| `shouldStopAfterTurn` | After `prepareNextTurn`, before steering/follow-up polling and before the next LLM call. | `TurnPolicy::should_stop`. |
| `waitForIdle()` | Resolves after run completion and after awaited `agent_end` subscribers settle. | `AgentHandle::wait_for_idle`, a run barrier including event-sink acknowledgements. |
| `reset()` | Rejects while active. Clears messages, streaming state, pending calls, error, and queues; retains configured model, system prompt, tools, callbacks, and options. | `Agent::reset_transcript`, idle-only, with the same retention semantics. A separate `reset_all` may restore builder defaults. |
| `setDefaultStreamFn` / process-global default | Provides a global fallback stream function used by low-level and compatibility paths. | **Not carried.** Replace with explicit `Arc<dyn ModelRuntime>` injection and `AgentFactory`. Global mutable request routing is hostile to tests, multiple tenants, and FFI. |
| `prompt(string, images)` | Constructs one user message whose content begins with text followed by images. | `prompt_text(PromptText { text, images })`. |
| `prompt(message)` | Adds one supplied message and emits start/end for it. | `prompt_records([record])`. |
| `prompt(message[])` | Adds and emits all prompt messages in order. | `prompt_records(records)`. |
| `continue()` | Adds no initial message. Context must end in user/tool-result, except high-level Agent first drains queued steering/follow-up when the tail is assistant. | `continue_run`. Same precondition and queue behavior. |
| retry after failed assistant | README implies `continue`, but actual low-level/high-level code rejects assistant-tail continuation unless a queue supplies a new message. | **Deliberate addition:** `retry_last_turn`, retaining the failed record but excluding it from request projection. |
| `abort()` | Aborts active run. | `AgentControl::cancel(run_id)` or cancellation token. |
| `subscribe()` | High-level Agent awaits listeners in registration order; low-level `agentLoop` stream is observational and does not create barriers. | Low-level event stream is observational. `AgentHandle` supports ordered acknowledged sinks for barrier semantics. |
| `state.streamingMessage` | Current mutable partial assistant message. | `AgentSnapshot.streaming: Option<AssistantMessageSnapshot>` built by `AssistantAssembler`. |
| `state.pendingToolCalls` | IDs of currently pending tool calls. | `AgentSnapshot.pending_tool_calls: Arc<[ToolCallId]>`. |
| tool execution `"parallel"` | Preflight sequential; allowed executions concurrent; completion events by completion order; transcript tool results by assistant source order. | `ToolScheduler` with explicit `PreflightIndex`, `CompletionIndex`, and `SourceIndex`. |
| per-tool `"sequential"` | If any call targets a sequential tool, the whole batch becomes sequential. | `ToolExecutionPlan::SequentialBatch` under the same rule. |
| `terminate: true` | Stops automatic continuation only when every finalized result in the batch terminates. | `ToolBatchOutcome::terminate = all(results.terminate)`. |

The queue and turn ordering comes directly from `packages/agent/src/agent-loop.ts:1–420`; tool ordering comes from `agent-loop.ts:420–760`.

The high-level queue, idle, reset, prompt, and continue semantics are in `packages/agent/src/agent.ts:1–460`.

## 8.2 Rust phase model

```rust
pub enum AgentPhase {
    StartRun,

    InitialQueuePoll,
    InjectPendingMessages,

    PrepareContext,
    RequestAssistant,
    CommitAssistant,

    PrepareToolBatch,
    ExecuteToolBatch,
    CommitToolResults,

    FinishTurn,
    PrepareNextTurn,
    ShouldStopAfterTurn,

    PollSteering,
    WouldStop,
    PollFollowUp,

    FinishRun,
}
```

The critical order is:

```text
CommitAssistant
    ↓
PrepareToolBatch
    ↓
Execute every tool in the batch
    ↓
Commit tool results in source order
    ↓
FinishTurn
    ↓
PrepareNextTurn
    ↓
ShouldStopAfterTurn
    ↓
PollSteering
    ↓
if otherwise stopping: PollFollowUp
```

> Correction: The summarized `Execute every tool in the batch` → `Commit tool results in source order` sequence applies to parallel batches. In sequential execution, and in the dedicated length-truncated-call synthesis path, pinned Pi emits `tool_execution_end` and the tool-result message lifecycle for each call before starting the next call. Parallel execution still defers all tool-result messages until its joined executions settle, then emits them in assistant source order (`packages/agent/src/agent-loop.ts:386–403,444–480,499–548`).

A steering message does not interrupt an already executing tool batch. "Interrupt" in Pi means it changes the next model turn after current tools settle. The README describes this ordering explicitly. See `packages/agent/README.md:250–520`.

> Correction: For a prompt run, `InitialQueuePoll` occurs after `RunStarted`, `TurnStarted`, and the initial prompt's message lifecycle and commitment, then its drained steering records are injected before `PrepareContext`. The phase list above is not chronological at that boundary. This matches pinned Pi, where `runAgentLoop` emits `agent_start`, `turn_start`, and prompt `message_start`/`message_end` before `runLoop` performs its initial `getSteeringMessages` poll (`packages/agent/src/agent-loop.ts:109–115,166`).

## 8.3 Event sequences

### `prompt()` without tools

```text
RunStarted
TurnStarted

MessageStarted(user)
MessageCommitted(user)

MessageStarted(assistant)
AssistantUpdate*
MessageCommitted(assistant)

TurnFinished {
    assistant,
    tool_results: []
}

RunFinished
```

Pi names these:

```text
agent_start
turn_start
message_start(user)
message_end(user)
message_start(assistant)
message_update*
message_end(assistant)
turn_end
agent_end
```

### `prompt()` with tools

```text
RunStarted
TurnStarted

MessageStarted(user)
MessageCommitted(user)

MessageStarted(assistant-with-tool-calls)
AssistantUpdate*
MessageCommitted(assistant-with-tool-calls)

ToolExecutionStarted(call-0)
ToolExecutionUpdated(call-0)*
ToolExecutionFinished(call-0)

MessageStarted(tool-result-0)
MessageCommitted(tool-result-0)

TurnFinished {
    assistant,
    tool_results: [tool-result-0]
}

TurnStarted

MessageStarted(assistant-final)
AssistantUpdate*
MessageCommitted(assistant-final)

TurnFinished {
    assistant-final,
    tool_results: []
}

RunFinished
```

For parallel calls, `ToolExecutionFinished` follows actual completion order, while `MessageCommitted(tool-result)` follows assistant source order.

Pi's documented tool event sequence and ordering are in `packages/agent/README.md:80–250`.

### `continue()` without resulting tool calls

No initial user/tool-result message is re-emitted:

```text
RunStarted
TurnStarted
MessageStarted(assistant)
AssistantUpdate*
MessageCommitted(assistant)
TurnFinished
RunFinished
```

### `continue()` with resulting tool calls

```text
RunStarted
TurnStarted
MessageStarted(assistant-with-tool-calls)
AssistantUpdate*
MessageCommitted(assistant-with-tool-calls)
ToolExecutionStarted*
ToolExecutionUpdated*
ToolExecutionFinished*
MessageStarted(tool-result)*
MessageCommitted(tool-result)*
TurnFinished
TurnStarted
MessageStarted(assistant-final)
AssistantUpdate*
MessageCommitted(assistant-final)
TurnFinished
RunFinished
```

## 8.4 Concurrent queue producers with `run(&mut self)`

A bare `&mut Agent` cannot simultaneously receive method calls while `run()` holds the mutable borrow. Queue ingress therefore lives in a separate cloneable control handle.

```rust
pub struct Agent {
    state: AgentState,
    queue_rx: QueueReceiver,
    // ...
}

#[derive(Clone)]
pub struct AgentControl {
    queue_tx: QueueSender,
    cancellation: RunCancellationRegistry,
}
```

```rust
impl AgentControl {
    pub async fn steer(
        &self,
        message: AgentRecord,
    ) -> Result<QueueReceipt, ControlError>;

    pub async fn follow_up(
        &self,
        message: AgentRecord,
    ) -> Result<QueueReceipt, ControlError>;

    pub fn cancel(
        &self,
        run_id: RunId,
    ) -> Result<(), ControlError>;
}
```

The queue command carries a monotonic ingress sequence:

```rust
pub struct QueueCommand {
    pub sequence: QueueSequence,
    pub kind: QueueKind,
    pub message: AgentRecord,
}
```

The run owns the receiver and polls it only at defined phase boundaries.

For a durable harness, an enqueue is acknowledged only after a `QueueEnqueued` operation record is accepted by the session store. For a bare in-memory agent, acknowledgement means the bounded queue accepted it.

---

# 9. Runtime assumptions and `Send`

## 9.1 Core executor neutrality

The core does not need to spawn tasks to execute tools concurrently. It can poll all tool futures itself:

```rust
let mut running = FuturesUnordered::new();

for prepared in prepared_calls {
    running.push(execute_and_finalize(prepared));
}

while let Some(completed) = running.next().await {
    emit_completion_event(completed).await?;
}
```

This provides concurrency without detached tasks and ensures all tool work settles before `RunFinished`.

The core therefore need not depend on Tokio.

OS process management, timers, sockets, and an actor owner task belong to environment/runtime adapters.

## 9.2 Local and Send runtimes

Trying to make one trait serve both WASM-local and multithreaded native targets usually creates unnecessary `Send` constraints. Expose two object-safe families.

```rust
pub type LocalBoxFuture<'a, T> =
    Pin<Box<dyn Future<Output = T> + 'a>>;

pub type SendBoxFuture<'a, T> =
    Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type LocalBoxStream<'a, T> =
    Pin<Box<dyn Stream<Item = T> + 'a>>;

pub type SendBoxStream<'a, T> =
    Pin<Box<dyn Stream<Item = T> + Send + 'a>>;
```

### Local runtime

```rust
pub trait LocalModelRuntime: 'static {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>>;
}

pub trait LocalTool: 'static {
    fn spec(&self) -> &ToolSpec;

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Rc<dyn LocalToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, ToolError>>;
}
```

This supports:

* browser WASM;
* single-threaded embedded executors;
* mobile main-thread integrations;
* `Rc`-based host values.

### Send runtime

```rust
pub trait ModelRuntime: Send + Sync + 'static {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>>;
}

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> &ToolSpec;

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>>;
}
```

The returned stream is:

```rust
pub struct AssistantStream {
    inner: SendBoxStream<'static, AssistantEvent>,
}
```

A model runtime can return a `'static` stream because the stream owns the HTTP response body/client state. The future used to establish it only borrows the runtime.

## 9.3 Run stream lifetime

The low-level state machine can borrow the agent:

```rust
impl Agent {
    pub fn run<'a>(
        &'a mut self,
        input: AgentInput,
        cancellation: CancellationToken,
    ) -> SendBoxStream<'a, AgentEvent>;
}
```

Requirements:

* the run stream is not `'static`;
* no task must outlive the mutable borrow;
* tool futures are joined before termination;
* dropping the run stream triggers cancellation and drives cleanup according to the drop policy.

A local equivalent returns `LocalBoxStream<'a, AgentEvent>`.

## 9.4 Actor facade

The actor is a runtime adapter:

```rust
// pi-agent-runtime-tokio

pub struct TokioAgentHandle {
    command_tx: tokio::sync::mpsc::Sender<AgentCommand>,
    state_rx: tokio::sync::watch::Receiver<AgentSnapshot>,
}
```

The Tokio adapter owns one task containing the `Agent`. Commands are processed serially:

```text
Prompt
Continue
Retry
Steer
FollowUp
Cancel
Reset
Snapshot
Shutdown
```

The actor may stream events through bounded channels, but the core still joins model/tool futures. There are no detached tool tasks that continue mutating the filesystem after a terminal event.

The actor requires:

```text
ModelRuntime: Send + Sync + 'static
Tool: Send + Sync + 'static
AgentEvent: Send + 'static
SessionStorage: Send + Sync + 'static
```

The local WASM facade can use `spawn_local` or host polling and the local traits.

## 9.5 Cancellation token

The core cancellation type should not be `tokio_util::sync::CancellationToken`.

A portable token can be implemented around:

* `Arc<AtomicBool>`;
* a waker list or event listener;
* child-token propagation.

```rust
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

impl CancellationToken {
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
    pub fn cancelled(&self) -> impl Future<Output = ()> + '_;
    pub fn child(&self) -> CancellationToken;
}
```

Tokio adapters can bridge it to Tokio cancellation where needed.

## 9.6 Honest runtime statement

The architecture should claim:

> `pi-agent-core` is executor-agnostic. The standard native harness runtime is Tokio-based. Process execution and the Send actor facade are provided by `pi-agent-runtime-tokio`. Local/WASM builds use the local runtime traits and do not compile the Tokio adapter.

It should not claim that the entire system is executor-neutral while shipping process and actor behavior only through implicit Tokio calls.

---

# 10. Operational definition of parity

Parity should be tracked in a machine-readable manifest pinned to the Pi commit:

```toml
upstream_repository = "earendil-works/pi"
upstream_commit = "c49906ec77788625aacbdc53ebca6fbe65bd20f5"

[[mapping]]
source = "packages/ai/test/transform-messages-copilot-openai-to-anthropic.test.ts"
rust = [
  "handoff::copilot_openai_thinking_becomes_text",
  "handoff::copilot_tool_signature_is_removed",
  "handoff::copilot_orphan_result_is_synthesized"
]
status = "semantic-parity"

[[mapping]]
source = "packages/agent/src/stream-fn.ts"
rust = ["architecture::no_global_stream_runtime"]
status = "deliberate-divergence"
reason = "Explicit ModelRuntime injection replaces mutable process-global fallback."
```

CI must fail when:

* an upstream `packages/ai/test/**/*.test.ts` file is absent from the manifest;
* an upstream `packages/agent/test/**/*.test.ts` file is absent;
* a mapped Rust test does not exist;
* a divergence lacks an explanation;
* the pinned source commit changes without regenerating the manifest.

The pinned Pi test directories contain the AI test inventory and the agent root tests, including `agent-loop.test.ts`, `agent.test.ts`, `e2e.test.ts`, `proxy.test.ts`, and harness tests.

## 10.1 `pi-ai` stream conformance

| Rust conformance test | Required behavior | Pi basis |
| --- | --- | --- |
| `stream_start_precedes_content` | No content event before message start. | `types.ts:300–560`; API stream implementations. |
| `stream_exactly_one_terminal` | One of Finished, Failed, Cancelled exactly once. | `types.ts:300–560`. |
| `stream_no_event_after_terminal` | Stream is fused after terminal. | Pi event-stream contract. |
| `stream_failure_is_terminal_message` | Error carries final assistant message with partial content. | `types.ts:300–540`; `anthropic-messages.ts:600–790`; `openai-completions.ts:500–790`. |
| `stream_cancellation_is_terminal_message` | Aborted message preserves content and usage. | Same sources. |
| `stream_partial_identity_is_stable` | Message ID and block IDs do not change during assembly. | Rust strengthening of Pi mutable-partial behavior. |
| `stream_response_id_is_preserved` | Response/message ID survives terminal assembly and persistence. | `types.ts:300–540`; OpenAI source. |
| `stream_response_model_is_preserved` | Concrete response model survives when different from requested. | `types.ts:300–540`; `openai-completions.ts:500–790`. |
| `stream_usage_is_cumulative` | Usage updates cannot be double-counted as deltas. | API usage handlers. |
| `stream_tool_json_scratch_not_persisted` | Partial JSON parser buffer never reaches message persistence. | Anthropic/OpenAI stream finalization. |
| `stream_binary_scratch_not_persisted` | Bedrock redacted chunk vectors become opaque bytes only. | `bedrock-converse-stream.ts:650–860`. |
| `stream_missing_provider_terminal_fails` | Premature end becomes failed assistant. | OpenAI Responses terminal checks. |
| `stream_error_sanitizes_secrets` | Public error excludes keys/auth headers/body secrets. | Provider error handling and native hardening. |
| `stream_unicode_matches_pi` | Surrogate sanitation produces Pi-equivalent request text. | API encoders' `sanitizeSurrogates`. |

The Pi stream type and canonical fields are defined in `packages/ai/src/types.ts:300–560`.

## 10.2 Opaque replay conformance

### Anthropic

```text
anthropic_signature_fragments_append_in_order
anthropic_signature_survives_message_round_trip
anthropic_turn_two_replays_exact_signature
anthropic_redacted_thinking_replays_exact_data
anthropic_unsigned_thinking_falls_back_to_text
anthropic_empty_signature_respects_compat
anthropic_failed_partial_signature_is_not_replayed
anthropic_signature_never_crosses_model_boundary
```

Basis: `anthropic-messages.ts:600–790` and `1200–1450`.

### OpenAI-compatible Completions

```text
openai_chat_reasoning_field_name_is_preserved
openai_chat_reasoning_details_preserve_array_order
openai_chat_reasoning_details_survive_round_trip
openai_chat_reasoning_details_replay_exact_json
openai_chat_block_signature_precedes_legacy_tool_signature
openai_chat_legacy_tool_signature_imports_as_replay_item
openai_chat_thinking_as_text_compat
openai_chat_reasoning_content_required_compat
openai_chat_incomplete_reasoning_detail_is_not_replayed
```

Basis: `openai-completions.ts:500–790` and `1050–1300`.

### OpenAI Responses

```text
responses_response_id_survives_round_trip
responses_reasoning_item_preserves_full_json
responses_reasoning_encrypted_content_survives
responses_output_items_preserve_global_order
responses_text_item_id_survives
responses_text_phase_survives
responses_function_call_call_id_survives
responses_function_call_item_id_survives
responses_function_call_namespace_survives
responses_different_model_drops_paired_item_id
responses_foreign_function_item_id_is_normalized
responses_incomplete_output_item_is_not_replayed
responses_turn_two_input_items_match_pi_order
```

Basis: `openai-responses-shared.ts:40–280` and `560–790`.

### Bedrock

```text
bedrock_redacted_chunks_concatenate_as_bytes
bedrock_redacted_bytes_survive_json_round_trip
bedrock_turn_two_replays_redacted_content_bytes
bedrock_signed_reasoning_replays_text_and_signature
bedrock_missing_required_signature_falls_back_to_text
bedrock_non_anthropic_model_omits_reasoning_signature
bedrock_partial_redacted_payload_is_not_replayed
```

Basis: `bedrock-converse-stream.ts:650–860` and `900–1045`.

### Google and Vertex

```text
google_thought_flag_not_signature_defines_thinking
google_text_part_signature_stays_on_text_part
google_thinking_part_signature_stays_on_thinking_part
google_tool_call_signature_stays_on_function_call
google_empty_signed_text_part_is_retained
google_empty_signed_thinking_part_is_retained
google_stream_omission_does_not_clear_prior_signature
google_invalid_base64_signature_is_dropped
google_signature_requires_same_provider_and_model
google_signature_never_moves_between_parts
```

Basis: `google-shared.ts:1–220`.

## 10.3 Retry conformance

```text
retry_x_should_retry_true_overrides_status
retry_x_should_retry_false_overrides_status
retry_transport_failure_without_status
retry_http_408
retry_http_409
retry_http_429
retry_http_500_through_599
retry_non_retryable_4xx
retry_after_ms_precedes_retry_after
retry_after_accepts_decimal_seconds
retry_after_accepts_http_date
retry_server_delay_over_max_fails_immediately
retry_zero_max_delay_disables_cap
retry_exponential_sequence_matches_pi
retry_jitter_range_matches_pi
retry_cancellation_before_attempt
retry_cancellation_during_request
retry_cancellation_during_backoff
retry_never_restarts_after_semantic_event
retry_fresh_transport_attempt_number
```

Basis: `packages/ai/src/utils/provider-retry.ts:1–260`.

## 10.4 Middleware conformance

```text
headers_merge_case_insensitively
headers_auth_before_model
headers_model_before_explicit
headers_explicit_before_transform
headers_transform_can_delete_default
headers_transform_runs_once
headers_transform_not_forwarded_to_provider_options

payload_in_place_mutation_is_retained
payload_replacement_is_retained
payload_transforms_run_in_registration_order
payload_transform_runs_once_per_logical_request
attempt_middleware_runs_per_retry
response_observer_runs_before_body_consumption
response_observer_runs_for_retry_responses
injected_http_transport_receives_final_request

bedrock_custom_headers_are_inserted_before_signing
bedrock_reserved_headers_are_suppressed
bedrock_response_observer_receives_raw_headers
```

Pi's Models-level header ordering is documented in the supplied provider notes. Pi's request callbacks and injected fetch are defined in `packages/ai/src/types.ts:80–300`.

## 10.5 Simple lowering conformance

```text
simple_context_reserves_4096_tokens
simple_context_clamp_never_returns_zero
simple_max_output_respects_model_limit
simple_model_sampling_defaults_apply
simple_request_sampling_overrides_model_defaults
simple_api_patch_overrides_common_simple_field
simple_typed_and_erased_patch_conflict
simple_unknown_api_patch_rejected

reasoning_xhigh_clamps_in_pi_mode
reasoning_xhigh_rejects_in_strict_mode
reasoning_explicit_unsupported_is_not_treated_as_missing
thinking_budget_defaults_match_pi
thinking_budget_reserves_1024_answer_tokens
thinking_budget_expands_explicit_answer_cap
thinking_budget_respects_model_max_output

anthropic_adaptive_uses_effort
anthropic_budget_model_uses_budget_tokens
anthropic_temperature_omitted_while_thinking
anthropic_temperature_omitted_when_model_disallows_it
anthropic_disabled_thinking_respects_compat

openai_compat_is_detected_from_effective_base_url
openai_model_compat_overrides_url_detection
openai_max_tokens_field_matches_compat
openai_reasoning_format_matches_compat
openai_thinking_budget_field_matches_compat
openai_sampling_params_merge_after_named_fields
```

Basis: `simple-options.ts:1–360`, `anthropic-messages.ts:800–1135`, and `openai-completions.ts:500–790`.

## 10.6 Handoff conformance

```text
handoff_null_content_normalized
handoff_nonvision_user_image_replaced
handoff_nonvision_tool_image_replaced
handoff_adjacent_image_placeholders_collapsed
handoff_failed_assistant_omitted
handoff_aborted_assistant_omitted
handoff_redacted_thinking_retained_exact_model
handoff_redacted_thinking_dropped_cross_model
handoff_signed_empty_thinking_retained_exact_model
handoff_visible_thinking_becomes_plain_text_in_pi_mode
handoff_visible_thinking_becomes_tagged_text_when_configured
handoff_text_signature_dropped_cross_model
handoff_tool_signature_dropped_cross_model
handoff_tool_id_normalized
handoff_matching_tool_result_id_rewritten
handoff_tool_id_collision_gets_stable_hash
handoff_missing_tool_result_synthesized
handoff_existing_tool_result_not_duplicated
handoff_multiple_missing_results_preserve_source_order
handoff_loss_report_contains_every_drop
handoff_strict_mode_rejects_lossy_projection
```

Basis: `transform-messages.ts:1–380` and the Copilot migration test.

## 10.7 Catalog and auth conformance

### Catalog

```text
catalog_reads_last_published_snapshot_synchronously
catalog_static_refresh_is_noop
catalog_restore_precedes_auth_resolution
catalog_restore_precedes_network
catalog_network_refresh_is_best_effort_per_provider
catalog_superseded_refresh_cannot_publish
catalog_persist_precedes_publish
catalog_failed_persist_keeps_old_snapshot
catalog_reader_never_sees_partial_candidate
catalog_host_override_applies_after_dynamic_snapshot
catalog_removed_override_reveals_provider_value
catalog_raw_snapshot_does_not_contain_flattened_override
catalog_typed_compat_mismatch_is_rejected
catalog_unknown_extensions_round_trip
catalog_runtime_override_has_highest_precedence
```

Basis: `models.ts:250–760` and the documented dynamic-provider contract.

### Authentication

```text
auth_explicit_request_value_wins
auth_stored_credential_owns_provider
auth_environment_used_only_without_stored_credential
auth_failed_oauth_refresh_never_falls_back_to_env
auth_oauth_refresh_is_serialized
auth_login_persists_under_modify
auth_list_never_resolves_secrets
auth_text_prompt
auth_secret_prompt
auth_select_returns_option_id
auth_manual_code_can_be_cancelled_by_callback
auth_device_default_interval_is_five_seconds
auth_device_interval_minimum_is_one_second
auth_device_slow_down_adds_five_seconds
auth_device_server_interval_wins
auth_device_deadline_is_enforced
auth_device_poll_is_cancellable
auth_pkce_state_is_validated
auth_callback_and_manual_first_valid_wins
auth_late_losing_response_is_superseded
auth_mobile_custom_scheme_flow
auth_mobile_unsupported_fixed_loopback_is_explicit
auth_provider_extra_fields_round_trip
```

Basis: `auth/types.ts:1–340`, `oauth/device-code.ts:1–240`, and `oauth/openai-codex.ts:120–520`.

## 10.8 Golden provider request bodies

This is a separate, mandatory parity level.

For each API family:

```text
wire_anthropic_messages_pi_exact
wire_openai_completions_pi_exact
wire_openai_responses_pi_exact
wire_openai_codex_responses_pi_exact
wire_azure_openai_responses_pi_exact
wire_google_generative_ai_pi_exact
wire_google_vertex_pi_exact
wire_bedrock_converse_stream_pi_exact
wire_mistral_conversations_pi_exact
wire_pi_messages_pi_exact
```

Each family needs fixtures covering:

```text
text-only
system/developer prompt
images
thinking disabled
each supported reasoning level
signed thinking replay
redacted/encrypted reasoning replay
one tool call
multiple tool calls
tool results
tool-result images
orphan-result repair
cache disabled/short/long
sampling defaults and overrides
max-output clamp
strict tool schema
provider/model headers
session affinity
API-specific compat flags
cross-provider handoff
failed-turn omission
```

### Byte-comparison contract

For a canonical context and fixed options:

```rust
let rust_request = rust_api.encode(fixture.context, fixture.options)?;
let pi_request = fixture.captured_pi_request;

assert_eq!(
    rust_request.body_bytes,
    pi_request.body_bytes,
    "provider request body differs byte-for-byte"
);
```

This requires an ordered JSON representation rather than serializing arbitrary `HashMap`s.

The compatibility JSON writer must reproduce relevant JavaScript `JSON.stringify` behavior:

* object insertion order;
* integer-like key ordering where applicable;
* no extra whitespace;
* omitted absent fields;
* exact string escaping;
* Pi-compatible number representation;
* stable array order;
* surrogate sanitation before encoding.

Random or environment-derived values must be injected deterministically in the fixture:

* request IDs;
* session IDs;
* timestamps;
* OAuth account IDs;
* temporary paths;
* auth tokens.

Authentication headers are redacted in stored captures, but the request body itself remains exact. URL/query and logical headers should have separate semantic or ordered-byte assertions as appropriate.

### Turn-two replay goldens

The most important fixtures are two-turn captures:

```text
turn 1 provider response event frames
    ↓
Rust AssistantAssembler
    ↓
serialize session
    ↓
deserialize session
    ↓
append tool result or user message
    ↓
encode turn 2
    ↓
assert byte-identical to Pi turn-2 body
```

Required families:

```text
anthropic_signed_thinking_turn_two_pi_exact
anthropic_redacted_thinking_turn_two_pi_exact
openai_chat_reasoning_details_turn_two_pi_exact
openai_responses_encrypted_reasoning_turn_two_pi_exact
bedrock_redacted_reasoning_turn_two_pi_exact
google_tool_thought_signature_turn_two_pi_exact
google_empty_signed_part_turn_two_pi_exact
```

No provider family is considered replay-capable until these pass.

## 10.9 `pi-agent-core` conformance

### Lifecycle

```text
agent_prompt_text_event_sequence
agent_prompt_message_event_sequence
agent_prompt_message_batch_event_sequence
agent_continue_event_sequence
agent_prompt_without_tools
agent_prompt_with_one_tool
agent_prompt_with_multiple_tools
agent_continue_without_tools
agent_continue_with_tools
agent_run_finished_is_final_event
agent_low_level_stream_is_observational
agent_handle_event_sinks_are_barriers
agent_wait_for_idle_includes_run_finished_sinks
```

### Failure and cancellation

```text
agent_failed_assistant_is_committed
agent_cancelled_assistant_is_committed
agent_partial_content_survives_failure
agent_partial_usage_survives_failure
agent_failed_turn_has_turn_finished
agent_failed_turn_has_run_finished
agent_no_tools_execute_after_failed_assistant
agent_failed_assistant_is_omitted_from_next_provider_projection
agent_continue_rejects_assistant_tail
agent_continue_drains_steering_before_rejecting_assistant_tail
agent_continue_drains_followup_after_steering
agent_retry_last_turn_reuses_last_valid_request_boundary
```

### Context phases

```text
agent_transform_context_runs_before_projector
agent_projector_runs_once_per_model_turn
agent_context_policy_receives_cancellation
agent_prepare_next_turn_runs_after_turn_finished
agent_prepare_next_turn_can_replace_context
agent_prepare_next_turn_can_replace_model
agent_prepare_next_turn_can_replace_reasoning
agent_should_stop_runs_after_prepare_next_turn
agent_should_stop_precedes_queue_poll
```

### Tools

```text
tool_unknown_name_becomes_error_result
tool_prepare_arguments_precedes_validation
tool_validation_precedes_before_hook
tool_before_hook_can_block
tool_before_hook_can_terminate
tool_execution_error_becomes_error_result
tool_updates_precede_tool_finished
tool_late_updates_are_ignored
tool_after_hook_precedes_tool_finished
tool_after_hook_can_replace_content
tool_after_hook_can_replace_details
tool_after_hook_can_replace_usage
tool_after_hook_can_change_error_state
tool_after_hook_can_terminate
tool_any_sequential_tool_forces_sequential_batch
tool_parallel_preflight_is_source_order
tool_parallel_completion_events_are_completion_order
tool_parallel_result_messages_are_source_order
tool_parallel_turn_results_are_source_order
tool_batch_terminates_only_when_all_results_terminate
tool_length_truncated_calls_are_never_executed
tool_length_truncated_calls_each_receive_error_result
tool_cancellation_stops_new_sequential_calls
tool_cancellation_joins_running_parallel_calls
tool_no_process_or_file_mutation_after_run_finished
```

### Queues

```text
queue_steering_polled_at_run_start
queue_steering_not_polled_between_tools
queue_steering_polled_after_completed_turn
queue_steering_polled_after_prepare_next_turn
queue_steering_not_polled_when_should_stop_returns_true
queue_followup_polled_only_when_agent_would_stop
queue_one_mode_drains_one
queue_all_mode_drains_all
queue_ingress_order_is_stable
queue_clear_steering
queue_clear_followup
queue_clear_all
queue_concurrent_producers_use_control_handle
```

### State management

```text
agent_reset_rejects_while_active
agent_reset_clears_transcript
agent_reset_clears_partial_state
agent_reset_clears_pending_tool_calls
agent_reset_clears_error
agent_reset_clears_queues
agent_reset_preserves_model
agent_reset_preserves_system_prompt
agent_reset_preserves_tools
agent_reset_preserves_runtime_and_policies
```

The Pi bases are `packages/agent/test/agent-loop.test.ts`, `agent.test.ts`, and the source phases in `agent-loop.ts` and `agent.ts`. The test files are present in the pinned agent test inventory.

## 10.10 Harness conformance

### Reducer and session tree

```text
session_sequence_starts_at_one
session_sequence_is_global_across_mutation_kinds
session_sequence_gap_is_corruption
session_entry_parent_must_exist
session_lane_head_moves_on_append
session_lane_can_move_to_ancestor
session_multiple_lanes_share_entry_tree
session_branch_scan_leaf_to_root
session_global_entry_query_sequence_order
session_fact_latest_value_wins
session_label_is_global_not_branch_scoped
session_stats_derive_from_usage_records
session_open_operation_detected
session_multiple_open_operations_is_corruption
session_operation_recovery_reconstructs_intent
session_reducer_replay_equals_live_state
```

### Pi v4 JSONL

```text
pi_v4_header_reads
pi_v4_unsupported_version_rejected
pi_v4_entry_mutation_reads
pi_v4_record_mutation_reads
pi_v4_lane_mutation_reads
pi_v4_fact_mutation_reads
pi_v4_unknown_entry_type_rejected
pi_v4_unknown_record_type_rejected
pi_v4_invalid_sequence_rejected
pi_v4_torn_final_line_repaired
pi_v4_midfile_corruption_not_silently_repaired
pi_v4_concurrent_append_is_serialized
pi_v4_existing_bytes_preserved_on_append
pi_v4_compatible_mutation_writes
pi_v4_unrepresentable_native_mutation_rejected
pi_v4_branch_fork_semantics
pi_v4_tree_fork_semantics
pi_v4_import_then_native_export_preserves_semantics
```

Pi has dedicated context, codec, storage, JSONL, memory, and search tests under `packages/agent/test/harness/session`.

### Compaction and branch summary

```text
compaction_threshold_decision
compaction_manual_reason
compaction_overflow_reason
compaction_retains_configured_tail
compaction_records_tokens_before
compaction_records_summary_usage
compaction_failure_does_not_move_branch_head
compaction_operation_can_resume
compaction_context_uses_latest_compaction_entry
branch_summary_finds_common_ancestor
branch_summary_summarizes_abandoned_segment
branch_summary_records_from_id
branch_summary_navigation_is_durable
branch_summary_failure_leaves_navigation_recoverable
```

Pi's harness test inventory includes `compaction.test.ts` and `branch-summarization.test.ts`.

### Environment and reference tools

```text
env_read_file
env_write_file
env_atomic_replace
env_process_stdout_stream
env_process_stderr_stream
env_process_exit_status
env_process_graceful_termination
env_process_forced_termination
env_process_tree_termination
env_stdio_grace_period
env_cancellation

mutation_queue_same_path_serializes
mutation_queue_different_paths_concurrent
edit_requires_exact_match
edit_rejects_multiple_matches
edit_rejects_noop
truncate_never_splits_utf8
truncate_respects_byte_limit
truncate_respects_line_limit
bash_truncated_output_has_full_artifact
```

Pi's harness test inventory includes `nodejs-env.test.ts` and subsystem tests; the source behaviors are also represented by the environment, file mutation queue, and truncation implementations.

### Skills, templates, and telemetry

```text
skill_catalog_discovers_valid_skills
skill_invalid_metadata_is_reported
skill_content_digest_is_stable
skill_resume_uses_recorded_digest
prompt_template_argument_substitution
prompt_template_missing_argument_rejected
prompt_template_output_is_deterministic

telemetry_schema_validates_every_event
telemetry_schema_version_is_required
telemetry_default_excludes_content
telemetry_default_excludes_auth
telemetry_default_excludes_replay_payload
telemetry_correlates_session_run_operation
telemetry_durable_event_follows_commit
telemetry_sink_failure_is_best_effort_by_default
```

The pinned harness tests include skills, prompt templates, events, reducers, resource formatting, and the top-level scaffold tests.

## 10.11 Deliberate divergence allowlist

Parity CI should explicitly permit only documented divergences:

| Divergence | Replacement |
| --- | --- |
| Mutable partial messages silently acquire provider signatures. | Explicit replay stream events plus `AssistantAssembler`. |
| Error handling represented only through Pi's event stream object. | In-band committed terminal message plus structured `RunOutcome`. |
| README ambiguity around retry via `continue()`. | Strict `continue_run` plus explicit `retry_last_turn`. |
| Global `setDefaultStreamFn`. | Explicit `ModelRuntime` injection and `AgentFactory`. |
| Silent lossy context transformations. | `HandoffReport` and optional strict rejection. |
| Provider replay values overloaded into three string fields. | Versioned ordered `ReplayEnvelope`; Pi importer/exporter handles legacy slots. |
| Node-owned loopback server inside provider OAuth implementation. | Host-provided `RedirectReceiver`; core owns OAuth state machine. |
| One implicit async/executor environment. | Local executor-neutral core plus explicit Send/Tokio adapter. |
| Pi v4 JSONL as the only session representation. | Native versioned log plus explicit Pi-v4 compatibility backend. |
| Top-level harness scaffold as implied completed behavior. | Implement orchestration against the already-defined subsystem contracts and tests. |

---

# Commitment gates

Before treating this architecture as committed, I would require four implementation gates:

1. **Replay gate:** all seven two-turn replay goldens pass after event assembly and persistence round-trip.
2. **Wire gate:** default request bodies for every supported API family are byte-identical to Pi for the pinned fixture corpus.
3. **Agent gate:** lifecycle, queue polling, tool scheduling, failed-message commitment, and event ordering pass the mapped Pi conformance suite.
4. **Session gate:** the Rust reader passes the Pi v4 codec/storage corpus, and the compatibility writer rejects rather than loses unrepresentable native state.

The central architecture remains:

```text
Models
    = provider/model/auth/catalog control plane
    + implementation of ModelRuntime

ModelRuntime
    = narrow model execution capability

Agent
    = state machine over ModelRuntime, tools, and policies

Harness
    = durable sessions, recovery, compaction, environment, skills,
      reference tools, and telemetry around Agent
```

The principal refinement is that **the boundary between `pi-ai` and `pi-agent-core` now includes a lossless, replay-aware assistant stream—not just text and tool deltas**. That is the point on which reasoning-model continuity, persistence, model handoff, FFI, and byte-faithful provider parity all depend.
