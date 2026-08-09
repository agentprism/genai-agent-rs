# NOTES — pi `file:line` → rust-genai-codex mapping

Source of truth ported here:
`pi/packages/ai/src/api/openai-codex-responses.ts` (the Codex `stream`), plus its
shared Responses processor `openai-responses-shared.ts`
(`processResponsesStream`). Every constant / behavior below cites the pi line it
was ported from; the same citations appear inline in the Rust code.

## Constants & URLs — `src/protocol.rs`

| Rust | Value | pi `file:line` |
|------|-------|----------------|
| `DEFAULT_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` | `DEFAULT_CODEX_BASE_URL`, openai-codex-responses.ts:59 |
| `JWT_CLAIM_PATH` | `https://api.openai.com/auth` | `JWT_CLAIM_PATH`, :60 |
| `OPENAI_BETA_SSE` | `responses=experimental` | :1622 |
| `OPENAI_BETA_WS` | `responses_websockets=2026-02-06` | `OPENAI_BETA_RESPONSES_WEBSOCKETS`, :827 (applied :1646) |
| `DEFAULT_ORIGINATOR` | `pi` | :1608 |
| `resolve_sse_url` | trim `/`, append `/codex/responses` unless already `/codex/responses` or `/codex` | `resolveCodexUrl`, :638-644 |
| `resolve_ws_url` | `resolve_sse_url` + scheme swap `https→wss`, `http→ws` | `resolveCodexWebSocketUrl`, :646-651 |
| WS connect timeout default | 15s | `DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS`, :64 |

### Headers

- Base headers — `Authorization: Bearer …`, `chatgpt-account-id`, `originator`,
  `User-Agent`: `buildBaseCodexHeaders`, :1592-1612. The default `User-Agent`
  reproduces pi's shape `pi (<platform> <release>; <arch>)` (:1609) — Rust's
  `std::env::consts::OS`/`ARCH` mapped to Node's names (`macos`→`darwin`,
  `windows`→`win32`; `x86_64`→`x64`, `aarch64`→`arm64`), with the OS release read
  from `/proc/sys/kernel/osrelease` on Linux (a constant elsewhere, since a
  portable read would need `unsafe`/libc `uname`). `with_user_agent(..)` overrides
  it. The live backend may reject a non-pi UA, hence the exact shape.
- SSE headers — base + `OpenAI-Beta: responses=experimental`,
  `accept: text/event-stream`, `content-type: application/json`, and (when a
  session/cache key is present) `session-id` + `x-client-request-id`:
  `buildSSEHeaders`, :1614-1632.
- WS headers — base + `x-client-request-id` + `session-id`, and **no**
  `OpenAI-Beta` / `accept` / `content-type`. pi *builds* `OpenAI-Beta` in
  `buildWebSocketHeaders` (:1634-1650) but `connectWebSocket` then **strips it**
  (`delete wsHeaders["OpenAI-Beta"]`, :1050), so it never reaches the WS
  handshake. Reproduced faithfully (`build_ws_headers` never sets it).
- WebSocket request id: `codexSessionId || uuidv7()`, :288 (we mint a fresh
  correlation id via `gen_request_id`).

### Error-body parsing — `parse_error_response`

`parseErrorResponse`, :1548-1573: for `usage_limit_reached|usage_not_included|rate_limit_exceeded`
codes or status 429 (:1560) the friendly ChatGPT usage-limit message **wins over**
`error.message` (pi throws `friendlyMessage || message`, :447); otherwise
`error.message`, otherwise the raw body.

## Request body — `src/request.rs`

`buildRequestBody`, :530-597 (+ `convertResponsesMessages` /
`convertResponsesTools`, openai-responses-shared.ts:136-380).

| Body field | Value | pi line |
|------------|-------|---------|
| `model` | model id | :555 |
| `store` | `false` | :556 |
| `stream` | `true` | :557 |
| `instructions` | system prompt or `"You are a helpful assistant."` | :558 |
| `input` | converted transcript (system prompt excluded) | :559 (`includeSystemPrompt:false`, shared:172) |
| `text.verbosity` | option or `"low"` | :560 |
| `include` | `["reasoning.encrypted_content"]` | :561 |
| `prompt_cache_key` | session/cache key (optional) | :562 |
| `tool_choice` | option or `"auto"` | :563 |
| `parallel_tool_calls` | `true` | :564 |
| `temperature` | when set | :567-569 |
| `service_tier` | when set | :571-573 |
| `tools` | `type:"function"` items | :575-581 (`convertResponsesTools`, shared:344-380) |
| `reasoning` | `{ effort, summary: option ?? "auto" }` | :583-593 |

Message conversion (`convertResponsesMessages`, shared:136-338):

- user text → `input_text`; user image → `input_image` data/URL (shared:191-208).
- assistant text → `message`/`output_text` with synthesized `msg_pi_{idx}` id
  (shared:225-245).
- assistant tool call → `function_call` with split `callId|itemId`, `fc_*` item
  id only (shared:246-283, drop-non-`fc_` id at :257-262).
- tool result → `function_call_output` with split `call_id` (shared:287-303); an
  empty result output becomes `"(no tool output)"` (`convertToolResultOutput`,
  shared:76-103, :88).

WS `response.create` frame: request body spread under `{ type:"response.create", … }`
(`build_ws_create_frame`), :1504.

## Event stream → assistant events — `src/events.rs`

Codex-specific normalization `mapCodexEvents`, :722-758; content mapping
`processResponsesStream`, openai-responses-shared.ts:416-740. Each Codex event is
translated to a genai `ChatStreamEvent` (or an in-band `Fail`) and folded through
`rust-genai-agent`'s `AssistantAccumulator` — the exact path `GenaiStreamFn` uses
— so the assistant event contract is identical.

| Codex event | → genai `ChatStreamEvent` / terminal | pi line |
|-------------|--------------------------------------|---------|
| `response.created` | `Start` (commits transport, emits `start`); also records `response.id` as the fallback terminal id | shared:580-581 |
| `response.output_item.added` (message/reasoning) | lazy (opened on first delta) | shared:582-583, 447-468 |
| `response.output_item.added` (function_call/custom) | `ToolCallChunk` (start) | shared:469-509 |
| `response.reasoning_summary_text.delta` / `reasoning_text.delta` | `ReasoningChunk` (tracked per `output_index`) | shared:584-613 |
| `response.reasoning_summary_part.done` | `ReasoningChunk("\n\n")` **only if a reasoning slot exists for that `output_index`** (`if (!slot) continue;`) | shared:594-596 |
| `response.output_text.delta` / `refusal.delta` | `Chunk` (text, tracked per `output_index`) | shared:614-633 |
| `response.function_call_arguments.delta` | `ToolCallChunk` (cumulative args) | shared:634-639 |
| `response.function_call_arguments.done` | `ToolCallChunk` (final args) | shared:640-650 |
| `response.custom_tool_call_input.{delta,done}` | `ToolCallChunk` | shared:651-661 |
| `response.output_item.done` | finalize tool args / reconcile text/reasoning **per `output_index`** (backfill a never-streamed block; append an authoritative correction tail) | shared:662-719 |
| `response.{completed,done,incomplete}` | `End` (success) or in-band `Fail` | :742-749 + shared:533-577, 720-721 |
| `error` (code `websocket_connection_limit_reached`) | `TransportFail` → pre-commit SSE fallback (WS) / terminal (after commit or SSE) | :70, :349, :701-703 |
| `error` (any other code) | `Fail("Codex error: …")` | :727-733 |
| `response.failed` | `Fail(error.message ?? "Codex response failed")` | :735-740 |

Per-`output_index` content tracking mirrors pi's per-slot bookkeeping
(`processResponsesStream`, shared:662-689): a second message/reasoning block that
arrives only via `response.output_item.done` (no prior delta) is backfilled
independently of other slots, so it is never dropped. Because the accumulator
concatenates deltas into one block, `output_item.done` emits only the suffix of
the item's authoritative text beyond what already streamed (mirroring pi's
`startsWith` slice for tool args, shared:647-649); an empty authoritative item
keeps the streamed text (pi's `... || slot.block.thinking`, shared:670).

Terminal status → outcome (`mapStopReason`, shared:742-772; `assertSuccessfulOutput`,
:117-124):

- `completed` / `in_progress` / `queued` / unknown → success; the accumulator
  infers `Stop` vs `ToolUse` from captured tool calls (`mapStopReason(undefined, hasToolCalls)`).
- `incomplete` + `max_output_tokens` → success `Length`; other reason →
  `Fail("Response incomplete: …")`.
- `failed` / `cancelled` (via a `response.completed`-shaped event) →
  `Fail("An unknown error occurred")` (pi's `assertSuccessfulOutput` fallback).

Usage (`finalizeResponse`, shared:541-557): `input_tokens`,
`input_tokens_details.{cached_tokens,cache_write_tokens}`, `output_tokens`,
`output_tokens_details.reasoning_tokens`, `total_tokens`.
**Convention difference:** this crate keeps OpenAI's inclusive `input_tokens` as
genai `prompt_tokens` (so `AgentUsage.input_tokens` includes cache reads, with
`cache_read_tokens` reported separately) to match
`AgentUsage::from(genai::Usage)` / `GenaiStreamFn`. pi instead subtracts
`cached + cache_write` from its own `Usage.input` (shared:549).

SSE frame parsing (`parseSSE`, :764-821): split on blank line, join `data:`
lines, skip `[DONE]` (`src/events.rs::SseDecoder`).

## Transport + fallback — `src/stream.rs`

`stream` export, :244-504.

- Transport selection `options?.transport || "auto"`, :300 — `Auto` mirrors pi's
  default (WebSocket with SSE fallback).
- WebSocket path :307-380; `response.create` send :1504; frame read
  `parseWebSocket` :1269-1385.
- SSE path :382-489; POST to `resolveCodexUrl`, :406.
- Fallback: only a **pre-commit transport** failure falls back to SSE
  (`websocketStarted` gate, :349-378). This includes an in-band
  `{"type":"error","code":"websocket_connection_limit_reached"}` frame — pi maps
  it to a `CodexApiError` whose `code` makes `isWebSocketConnectionLimitReachedError`
  true, so the pre-start catch falls through to SSE (:349, :701-703). Every OTHER
  application error is a non-transport error and is not retried
  (`isCodexNonTransportError`, :697-699); a failure after commit re-throws (:373),
  as does a connection-limit frame that arrives after commit. In the mapper this
  is a distinct `MappedItem::TransportFail`, surfaced pre-commit as
  `do_sse = true` (fresh accumulator) and post-commit / on the SSE path as a
  terminal `acc.fail`. Cancellation → in-band `Aborted` (:336-338, :483-485,
  `assertSuccessfulOutput` abort path :121-123 → `stream.push({type:"error", reason:"aborted"})` :496-499).
- Never-throw contract: all failures become a terminal `error`/`done` event
  (:490-500), never a thrown/returned error — this is also the
  `rust_genai_agent::StreamFn` contract.

## Auth — token source

The bearer + account id come from `rust-genai-auth`. Account id is decoded from
the bearer JWT claim `https://api.openai.com/auth.chatgpt_account_id`
(`extractAccountId`, :1579-1590) via `rust_genai_auth::jwt::extract_chatgpt_account_id`.
Per-request refresh mirrors pi refreshing on each call; the auth crate's
`CodexTokenResolver` owns expiry/refresh/persist with the double-refresh race fix.

## Explicit e2e gap

No test connects to `chatgpt.com`. Real end-to-end validation needs a ChatGPT
subscription + live OAuth + network and is out of scope for CI. Mock servers
cover protocol framing, headers, event mapping, fallback, and cancellation only.
