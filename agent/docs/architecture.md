# rust-genai-agent — Architecture and Implementation Record

> **Legacy crate.** This documents `rust-genai-agent`, an earlier, deliberately partial agent loop built on the `genai` fork. It is **not** the pi-agent-core port defined by `docs/porting-pi-ai-and-agent-core-docs/goal.md`, which will be built on the `ai` crate to a full-parity standard; its scope exclusions, genai-foundation gap tables, and version pins are not that port's standard and may be stale. pi's pinned source — not this document — is authority.

> Port of `pi/packages/agent` (`@earendil-works/pi-agent-core`, TypeScript) to Rust,
> layered on the `genai` crate (rust-genai) the same way pi-agent-core is layered on `pi-ai`.
>
> **Scope: everything in pi-agent-core _except_ `src/harness/`** (durable sessions, compaction,
> reducer, reference tools, skills, prompt templates, telemetry, and the Node entry point are out of
> scope; no feature silently supplies them).

> **Current status — M1-M6 implementation complete, later parity batches landed.** The pinned
> non-harness matrix is **56/56 mapped and green** at pi commit `581d75a89…`; the latest sync adds
> the `proxy.test.ts` `toolcall_end` metadata case and pi-ai's `ToolCall.namespace`. The M1-M6
> completion checkpoint was **112/112 green**; release-hardening, production-parity,
> core-runtime, execution-seam, and latest pi-sync additions bring the current agent-crate
> all-feature suite to **206/206**, while the workspace's 7 FFI tests bring the agent + FFI gate
> to **213/213**. T0-T2 below are retained only as a historical test-first record; they do not
> describe the current runtime.

## 1. What is being ported (source inventory)

| pi-agent-core source | Lines | Port? | rust-genai-agent module |
|---|---|---|---|
| `src/types.ts` | 437 | ✅ | `message.rs`, `event.rs`, `tool.rs`, `hooks.rs`, `config.rs` |
| `src/agent-loop.ts` | 792 | ✅ | `agent_loop.rs` (+ `tool_exec.rs`) |
| `src/agent.ts` | 588 | ✅ | `agent.rs` |
| `src/stream-fn.ts` | 20 | ✅ | `stream_fn.rs` |
| `src/proxy.ts` | 369 | ✅ (feature `proxy`) | `proxy/` |
| `src/index.ts` exports | 136 | ✅ core exports (telemetry/UUID conveniences omitted; §8) | `lib.rs` |
| `src/harness/**` | ~8,300 | ❌ | — (excluded from this crate) |
| `src/node.ts` | 2 | ❌ | — |

## 2. Design spine (invariants carried over from pi-agent-core)

1. **The only coupling to the LLM layer is one injectable `StreamFn`.**
   pi-agent-core has zero provider dependencies; hosts inject `(model, context, options) → AssistantMessageEventStream`.
   We keep this: the agent loop never names `genai::Client`. A `GenaiStreamFn` adapter is provided, and a
   process-wide default can be installed via `set_default_stream_fn` (port of `stream-fn.ts`).
2. **Widened transcript type with a one-way bridge.**
   `AgentMessage` supersedes the LLM message union (adds custom app messages); `convert_to_llm` runs once
   per LLM call, after an optional `transform_context`. The loop works with `AgentMessage` throughout.
3. **Never-throw contracts.**
   `StreamFn` must encode failures in-band (final assistant message with stop-reason error/aborted), hooks
   must not unwind across the loop, tool failures become error tool-results. In Rust: the loop returns
   `Result` only for *programming* errors (loop guards); all runtime failures flow through events.

### pi-ai → genai foundation mapping

| pi-ai | genai |
|---|---|
| `Model<Api>` (rich catalog metadata) | `ModelSpec` / `ModelIden` (identifier only — no cost/context-window metadata; see §8) |
| `Context { systemPrompt, messages, tools }` | `ChatRequest { system, messages, tools }` |
| `Message = User \| Assistant \| ToolResult` | `ChatMessage { role, content: MessageContent }` |
| `Tool<TSchema>` (typebox) | `genai::chat::Tool { name, description, schema: Value, strict, … }` |
| `ToolCall { id, name, arguments: object }` | `genai::chat::ToolCall { call_id, fn_name, fn_arguments: Value, thought_signatures }` |
| `ToolResultMessage` (text/image parts) | `ToolResponse { call_id, fn_name, content: String }` (string only — see §8) |
| `streamSimple` / `StreamFn` | `Client::exec_chat_stream(model, ChatRequest, Option<&ChatOptions>)` |
| `AssistantMessageEventStream` (events + `result()`) | `ChatStream: Stream<Item = Result<ChatStreamEvent>>` (+ `StreamEnd` captures) |
| `Usage { input, output, cacheRead, cacheWrite, cost }` | `Usage { prompt_tokens, completion_tokens, *_details }` (no cost — genai has no price catalog) |
| `StopReason` stop/length/toolUse/error/aborted | `StopReason::{Completed, MaxTokens, ToolCall, ContentFilter, StopSequence, Other(raw)}` |
| `ThinkingLevel` + `ThinkingBudgets` | `ChatOptions::reasoning_effort: ReasoningEffort::{Zero, Minimal, Low, Medium, High, XHigh, Max, Budget(u32)}` |
| `getApiKey(provider)` per call | `Client::builder().with_auth_resolver[_async]_fn(...)` at client construction |
| `sessionId` (cache affinity) | `StreamRequest::session_id` — an independent request field forwarded from `AgentConfig`/`AgentLoopConfig`; **not** collapsed into `ChatOptions::prompt_cache_key`, which remains the explicit provider cache key |
| `validateToolArguments` (typebox + coercion) | `jsonschema` crate + ported coercion pass (§6) |
| `AbortSignal` | `tokio_util::sync::CancellationToken` |

## 3. The keystone: `assistant_stream.rs` — genai `ChatStream` → pi-ai-style assistant event protocol

pi-agent's loop consumes **events that each carry a partial `AssistantMessage`**, plus a `result()` future.
genai's `ChatStream` yields **bare chunks** (`Result<ChatStreamEvent>`) with final content/usage captured in
`StreamEnd` *only when requested* via `ChatOptions` capture flags. Bridging this is the heart of the port.

### Our types (mirroring pi-ai)

```rust
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,           // Text{text,signature} | Thinking{thinking,signature} | ToolCall{...}
    pub stop_reason: StopReason,                  // Pending | Stop | Length | ToolUse | Error | Aborted (+ raw provider string kept)
    pub error_message: Option<String>,
    pub usage: AgentUsage,                        // normalized from genai Usage; cost absent (see §8)
    pub model: ModelIden,                         // adapter + model name (≈ pi's api/provider/model fields)
    pub response_id: Option<String>,
    pub timestamp: i64,
}

pub enum AssistantMessageEvent {          // = pi-ai's AssistantMessageEvent
    Start { partial: AssistantMessage },
    TextStart   { content_index: usize, partial: AssistantMessage },
    TextDelta   { content_index: usize, delta: String, partial: AssistantMessage },
    TextEnd     { content_index: usize, content: String, partial: AssistantMessage },
    ThinkingStart/Delta/End { … },
    ToolCallStart { content_index: usize, partial: AssistantMessage },
    ToolCallDelta { content_index: usize, delta: String, partial: AssistantMessage },
    ToolCallEnd   { content_index: usize, tool_call: AgentToolCall, partial: AssistantMessage },
    Done  { reason: StopReason, message: AssistantMessage },
    Error { reason: StopReason, error: AssistantMessage },
}
```

### The adapter: `GenaiStreamFn`

Holds `genai::Client` + base `ChatOptions`. On invocation:
1. Build `ChatRequest` from the loop's LLM context; **force `capture_content`, `capture_tool_calls`,
   `capture_usage`, `capture_reasoning_content` to `true`** (StreamEnd is our only source of the final message).
2. `client.exec_chat_stream(model, req, opts).await` — a *pre-flight* `Err` (auth resolution, request build)
   is converted into an in-band `Error` event, preserving the never-throw `StreamFn` contract.
3. Fold each `ChatStreamEvent` through a stateful **accumulator**:

| genai `ChatStreamEvent` | accumulator action → emitted event |
|---|---|
| `Start` | init partial → `Start` |
| `Chunk(text)` | append to current text part → `TextStart?` then `TextDelta` |
| `ReasoningChunk(s)` | append to current thinking part → `ThinkingStart?/ThinkingDelta` |
| `ThoughtSignatureChunk(s)` | buffer; attached to current thinking part / first tool call |
| `ToolCallChunk(tc)` | **cumulative snapshot**; `tc.fn_arguments` is `Value::String(raw_partial_json)` during streaming → salvage-parse (port of pi-ai `parseStreamingJson`) → upsert part at index → `ToolCallStart/ToolCallDelta` |
| `Heartbeat` | not forwarded (loop-level concern only) |
| `End(StreamEnd)` | final message from `captured_content` (args now parsed Values, thoughts prepended, ordering Thought→Text→ToolCall), `captured_usage`, `captured_stop_reason`, `captured_response_id` → close open parts (`TextEnd`/`ToolCallEnd`) → `Done` |
| `Err(genai::Error)` | partial.stop_reason = Error, error_message = Display(e) → `Error` |
| cancellation | token fires mid-stream → drop stream, `Error{ reason: Aborted }` (via `tokio::select!`) |

StopReason mapping: `Completed→Stop`, `MaxTokens→Length`, `ToolCall→ToolUse`, `StopSequence→Stop`,
`ContentFilter→Stop` (raw preserved), `Other(raw)→Stop` with raw preserved; plus `Error`/`Aborted` produced
only by the adapter. The loop only branches on `error | aborted | length`, matching pi-agent.

> The `proxy/` module reconstructs the same protocol: `stream_proxy` POSTs `{model, context, options}` and
> reconstructs the same event protocol from bandwidth-stripped SSE lines (port of `proxy.ts`'s
> `processProxyEvent` accumulator) — which validates that `StreamFn` is genuinely provider-neutral.

The proxy endpoint and HTTP client are a trusted network/resource boundary. Production deployments
must use HTTPS; plain HTTP is for loopback/development. The built-in client rejects URL userinfo and
disables redirects, while an injected client retains the caller's redirect policy. The wire DTO
excludes the proxy bearer token and resolved `ServiceTarget` endpoint/auth material, but forwarded
`extra_headers`/`extra_body` may intentionally contain application secrets. Tool argument handling is
bounded to depth 128, 1 MiB raw JSON and 4,096 deltas per tool call, and 16 MiB cumulative reparse
work per invocation. Whitespace-only SSE data is ignored for upstream compatibility. SSE framing
and assistant text buffering remain unbounded, so the proxy is not a sandbox for a hostile endpoint.

## 4. Transcript model: `message.rs`

```rust
pub enum AgentMessage {
    User(UserMessage),              // content: Vec<UserContent::{Text, Image(base64,mime)}>, timestamp
    Assistant(AssistantMessage),    // from §3
    ToolResult(ToolResultMessage),  // tool_call_id, tool_name, content: Vec<Text|Image>, details: Value,
                                    // usage: Option<AgentUsage>, added_tool_names, is_error, timestamp
    Custom(CustomMessage),          // { role: String, data: serde_json::Value } — the TS declaration-merging
                                    // extensibility point, as an open variant
}
```

`convert_to_llm: Arc<dyn Fn(&[AgentMessage]) -> BoxFuture<Vec<ChatMessage>>>` — required in loop config.
Default (Agent level): pass through user/assistant/toolResult, drop custom:

- user: text → `ContentPart::Text`; image → `ContentPart::Binary(Binary::from_base64)`
- assistant: text → `Text`; thinking → `ReasoningContent` (genai adapters hoist it back to provider wire
  format); tool calls → `ContentPart::ToolCall` with thought signatures on the first call
  (`ChatMessage::assistant_tool_calls_with_thoughts` semantics)
- tool result → `ToolResponse { call_id, fn_name, content }`: text parts joined; **images unsupported by
  genai `ToolResponse.content: String`** → v1 policy: skip image parts with a marker line (see §8)

## 5. Tool model: `tool.rs` + `validate.rs`

```rust
pub trait AgentTool: Send + Sync {
    fn spec(&self) -> ToolSpec;                    // name, description, JSON schema Value, strict, label
    fn execution_mode(&self) -> Option<ToolExecutionMode> { None }   // per-tool override
    fn prepare_arguments(&self, args: Value) -> Value { args }       // compat shim, default identity
    fn execute(&self, call: ToolCallCtx, cancel: CancellationToken, on_update: UpdateSink)
        -> BoxFuture<'_, Result<AgentToolResult, ToolError>>;
}
pub struct AgentToolResult { pub content: Vec<ToolResultContent>, pub details: Value,
    pub usage: Option<AgentUsage>, pub added_tool_names: Vec<String>, pub terminate: bool }
```

- `details` is **type-erased to `serde_json::Value`** (TS generics don't port; Value is the pragmatic pivot).
- `on_update` is scoped to the execution; updates after settle are ignored (port: `AtomicBool` gate) —
  pinned by pi-agent tests ("should ignore tool updates after the tool execution settles").
- Ergonomics: `tool_fn!` / `FnTool` builder adapting `async fn(Value) -> Result<Value>` closures.
- **Validation** (`validate.rs`): `jsonschema` crate against `spec.schema`, preceded by a coercion pass
  ported from pi-ai `coerceWithJsonSchema` (null→0/false/""/`null`, "true"↔true, number↔string, recursive
  object/array/union handling). Error format parity:
  `Validation failed for tool "{name}":\n  - path.to.field: message\n\nReceived arguments:\n{json}`.
  Validation failure = error tool-result, never a panic/throw.
- Unknown tool name → error tool-result (`Tool {name} not found`).

## 6. Loop: `agent_loop.rs` + `tool_exec.rs`

Free async fns (stateless, like `runAgentLoop`/`runAgentLoopContinue`):

```rust
pub async fn run_agent_loop(prompts: Vec<AgentMessage>, ctx: AgentContext, cfg: AgentLoopConfig,
    sink: &mut impl AgentEventSink, cancel: CancellationToken, stream_fn: Arc<dyn StreamFn>)
    -> Result<Vec<AgentMessage>, LoopError>
pub async fn run_agent_loop_continue(ctx, cfg, sink, cancel, stream_fn) -> Result<Vec<AgentMessage>, LoopError>
    // guards: empty context → Err; last message assistant → Err   (parity with TS throws)
```

`AgentLoopConfig` = model (`ModelSpec`), `convert_to_llm` (required), optional `transform_context`,
`should_stop_after_turn`, `prepare_next_turn` (swap context/model/thinking between turns),
`get_steering_messages`, `get_follow_up_messages`, `before_tool_call` (can block),
`after_tool_call` (field-merge overrides: content/details/is_error/usage/terminate),
`tool_execution` (default Parallel), plus request options (`ChatOptions` extras: temperature, max_tokens,
seed, stop_sequences, cache_control, `prompt_cache_key`…).

Faithful ports of the tricky semantics (all pinned by pi-agent's `test/agent-loop.test.ts`):

- **Event ordering**: single-writer sink; every emit awaited before proceeding (this makes assistant
  `message_end` a barrier before tool preflight — `before_tool_call` sees state that includes the
  assistant message).
- **Length-truncation guard**: stop_reason == Length ⇒ fail *all* tool calls in the batch with the
  "re-issue with complete arguments" error result; execute none.
- **Parallel mode**: preflight (prepare_arguments → validate → before_tool_call) **sequentially in source
  order**; allowed calls execute concurrently (`tokio::task::JoinSet`); `tool_execution_end` emitted in
  **completion order**; tool-result **messages** appended/emitted in **source order**. Any tool in the
  batch with `execution_mode == Sequential` (or global sequential) ⇒ whole batch sequential.
- **Hooks under parallelism**: `after_tool_call` runs inside each tool task (parity) — hook contexts get an
  immutable `Arc` snapshot of the pre-batch context (faithful: TS pushes batch results into context only
  after the whole batch returns). Tool tasks communicate with the loop over an `mpsc` so the loop task
  remains the sole event writer.
- **Terminate**: batch stops the loop only when *every* finalized result has `terminate == true`.
- **Steering** polled after each turn (and once up-front); **follow-ups** polled when the agent would stop;
  outer/inner loop structure mirrors `runLoop`.
- **Cancellation**: token checked at the same points TS checks `signal.aborted` (after prepare, after
  before-hook, between sequential calls); aborted mid-stream ⇒ error/abort assistant message ends the run
  with `turn_end` + `agent_end`.
- **Error stop**: stop_reason Error/Aborted ⇒ `turn_end` + `agent_end`, no tool execution.

## 7. Stateful wrapper: `agent.rs`

```rust
pub struct Agent { /* state: RwLock<AgentState>, listeners, queues, active run handle, config */ }
```

- `AgentState { system_prompt, model, thinking_level, tools, messages, is_streaming,
  streaming_message, pending_tool_calls: HashSet<String>, error_message }` — read via `state()` snapshot;
  setters replace (clone-on-assign parity).
- `PendingMessageQueue` port with `QueueMode::{All, OneAtATime}` drain semantics (default one-at-a-time);
  `steer()`, `follow_up()`, `clear_*`, `has_queued_messages()`.
- `subscribe(listener)` — listeners are `Arc<dyn Fn(&AgentEvent, CancellationToken) -> BoxFuture<()>>`,
  **awaited in registration order**, and their settlement is part of run settlement (`agent_end`
  listeners included). (A `tokio::sync::broadcast` adapter can be layered on later for UI ergonomics, but
  the callback model is the semantic-faithful primary API.)
- `prompt(impl Into<PromptInput>)` — text(+images) / single message / batch; **`continue_()`** with full
  parity: busy ⇒ error; empty ⇒ error; last-assistant ⇒ drain steering (skip initial poll) then follow-ups
  before erroring; else continuation.
- `abort()` (per-run `CancellationToken`), `wait_for_idle()` (resolves after run + awaited listeners),
  `reset()`.
- **Failure path** (`handleRunFailure` port): loop-level panic/Err ⇒ synthesize an empty assistant message
  with stop_reason error/aborted + error_message, emit `message_start/end` + `turn_end` + `agent_end`,
  set `state.error_message`. pi's `EMPTY_USAGE` equivalent.
- `model` in state is `ModelSpec` (genai has no catalog metadata — see §8).
- `thinking_level: ThinkingLevel::{Off, Minimal, Low, Medium, High, XHigh, Max}` →
  Off ⇒ `reasoning_effort` unset; others map 1:1 to `ReasoningEffort`; budgets ⇒ `ReasoningEffort::Budget`.

## 8. Known gaps & decisions

| # | Topic | Decision |
|---|---|---|
| 1 | **Tool-call streaming granularity** | genai `ToolCallChunk` = cumulative snapshot with raw-JSON-string args → salvage-parse per chunk; final parse at `StreamEnd`. Preserves pi's toolcall_delta UX. |
| 2 | **Images in tool results** | The agentprism genai fork widens `ToolResponse` with optional binary `parts` (upstream PR #277): text parts join into `content`, image parts attach as `Binary` attachments. (v1 replaced images with an `[image omitted]` marker while `ToolResponse` was text-only.) |
| 3 | **`getApiKey` per-call** | Not per-call in genai; expiring tokens are handled natively via `Client::builder().with_auth_resolver_async_fn`. Document as the pattern; no agent-level hook. |
| 4 | **`onPayload` / `onResponse`** | Ported via the agentprism genai fork's exec interceptors (`PayloadInterceptor`/`ResponseObserver`, upstream PR #277 lineage). `on_payload`/`on_response` hooks (`OnPayloadHook`/`OnResponseHook` + a body-free `StreamResponseInfo`) sit on `AgentConfig`/`StreamRequest` with `Busy`-guarded setters; the loop forwards them per request, and the facade snapshots them at run admission (idle replacement affects the next run; an in-flight run keeps its snapshot). Custom `StreamFn`s and the proxy honor the per-request hooks directly; `GenaiStreamFn` honors them through the fork's request-level `ExecOptions` (`exec_chat_stream_with_exec_options`): a request hook **replaces** the construction-time hook of its channel (installed via `with_exec_hooks`) for that execution only — the two never compose, and exactly one hook fires per channel per physical attempt, including retries and HTTP-error responses. An absent request hook inherits the construction default, matching pi's single construction-site wiring when no per-request override is set. |
| 5 | **Cost accounting in `Usage`** | genai has no price catalog → `AgentUsage` carries token counts only; `cost` omitted. |
| 6 | **Model metadata (context window, cost, capabilities)** | `ModelSpec` is an identifier only. Agent state holds `ModelSpec`; catalog features (compaction thresholds etc.) belong to the excluded harness layer anyway. |
| 7 | **`metadata`, samplingParams** | pi-specific options with no genai counterpart → omitted; `ChatOptions` extras (`extra_headers`, `extra_body`, `prompt_cache_key`) cover the practical cases. (`transport` is a forwarded advisory and `maxRetries`/`maxRetryDelayMs` are forwarded request fields honored by `GenaiStreamFn`'s retry layer — no longer omissions.) |
| 8 | **Custom message extensibility** | TS uses declaration merging → Rust: `AgentMessage::Custom { role, data: Value }` open variant; `convert_to_llm` decides their fate. |
| 9 | **Tool `details`/typed results** | Type-erased `serde_json::Value` (runtime JSON schema is the type story). |
| 10 | **Async traits** | `StreamFn`/`AgentTool`/hooks must be dyn-safe ⇒ `async_trait` (or hand-written `BoxFuture`); crate is tokio-only. |
| 11 | **Hook snapshots and infallibility** | Async hooks receive owned, cloned snapshots so futures can be `'static`; only the before-tool context is mutably borrowed for argument replacement. Legacy hook outputs are deliberately not `Result`-shaped; the tool channels additionally offer opt-in fallible forms (`AgentTool::try_prepare_arguments`, `TryBeforeToolCallHook`, `TryAfterToolCallHook`) whose `Err` becomes the call's in-band error tool result (preparation/before failures skip execution; an after-hook failure replaces the completed result without rolling back side effects). A fallible channel takes precedence over its legacy counterpart and they never both run for one call. A panic is a programming fault: `Agent` synthesizes an in-band failure lifecycle, spawned `agent_loop` returns `LoopError::TaskPanicked` through its result handle, and direct `run_agent_loop` callers own normal Rust unwind handling. |
| 12 | **Unbounded observer/update handoff** | Spawned loop events and internal parallel tool-task updates use unbounded channels so a dropped/slow observer cannot change execution ordering or deadlock tasks. Producers must bound update frequency themselves; use the awaited `run_agent_loop` sink when consumer backpressure is required. |
| 13 | **`UpdateSink` re-entry** | `emit` and `close` are serialized across clones, and the synchronous callback executes while the shared gate is held. The callback must not call any method on the same sink or a clone; same-sink re-entry would deadlock. |
| 14 | **Convenience exports** | Telemetry setup and UUID generation exposed by the TypeScript index are intentionally omitted rather than imposing ecosystem choices; applications select their own crates. |
| 15 | **Harness and Node scope** | `src/harness/**` and `src/node.ts` are excluded, not deferred runtime features. This crate has no durable-session/compaction harness and no Node binding. |
| 16 | **Proxy trust and resource policy** | URL userinfo is rejected; the default HTTP client disables redirects; injected clients retain caller policy. Use HTTPS outside loopback/dev and trust the endpoint. The DTO excludes proxy/`ServiceTarget` credentials but can forward secret-bearing `extra_headers`/`extra_body`. Tool JSON is capped (depth 128; 1 MiB raw and 4,096 deltas per tool; 16 MiB cumulative reparse work per invocation), while SSE/text buffering deliberately remains unbounded. |

## 9. Current package and crate shape

The manifest pins the MSRV and uses Cargo's dual-source dependency form over the **single fork
version** (`=0.7.0-beta.19.2-agentprism`): local workspace builds use the sibling `genai/`
subtree checkout via `path`, while the packaged form `cargo publish` uploads resolves
`genai-agentprism` from crates.io by exact version pin. Both crates live in the
[genai-agent-rs workspace](https://github.com/agentprism/genai-agent-rs) and are **published to
crates.io** — the fork as `genai-agentprism` (lib target still named `genai`) and this crate as
`rust-genai-agent` — so consumers need no `[patch.crates-io]` indirection. Because this crate
uses fork-only APIs, the pin is exact and the two crates are published in lockstep
(`genai-agentprism` first; cargo refuses to upload `rust-genai-agent` until the pinned fork
version is on the registry). The earlier interim distribution model — locally packaged archives
plus a consumer-side `[patch.crates-io]` entry, gated by `scripts/check-distribution.sh`
(DIST-01) while `publish = false` was in effect — is retired.

```toml
[package]
name = "rust-genai-agent"
version = "0.3.0"
edition = "2024"
rust-version = "1.88"
readme = "README.md"

[dependencies]
# Dual-source: `path` for local workspace builds, `version` (exact pin — this crate uses
# fork-only APIs) for the packaged form, which resolves genai-agentprism from crates.io.
genai = { package = "genai-agentprism", version = "=0.7.0-beta.19.2-agentprism", path = "../genai" }

[features]
default = []
testing = ["genai/testing"]
proxy = ["dep:eventsource-stream", "tokio/net", "tokio/io-util"]
```

```text
src/
  lib.rs              # crate docs and public re-exports
  message.rs          # AgentMessage, content types, and default conversion
  assistant.rs        # AssistantMessage/Event, StopReason, and token usage
  assistant_stream.rs # event stream plus accumulator adapter plumbing
  stream_fn.rs        # StreamFn, GenaiStreamFn, and process default
  tool.rs             # AgentTool, ToolSpec, FnTool, and UpdateSink
  validate.rs         # schema coercion and validation
  hooks.rs            # owned hook snapshots and hook result types
  config.rs           # loop/context/queue/tool/thinking configuration
  agent_loop.rs       # spawned and awaited stateless loop entry points
  tool_exec.rs        # sequential/parallel tool batch engine
  agent.rs            # stateful Agent, subscriptions, queues, and cancellation
  proxy/              # feature-gated HTTP/SSE StreamFn and frozen wire DTOs
  testing.rs          # feature-gated scripted offline support
  error.rs            # admission, loop, tool, stream, and validation errors
```

## 10. Historical test-first strategy (completed)

The pi-agent behavioral contract is its test suite, so the test matrix was ported **before** the
implementation, against an API skeleton. This provided: (a) a concrete alignment metric (green tests /
ported tests), (b) early API validation, and (c) regression protection from the first line of runtime code.
Everything in this section describes completed sequencing, not the current implementation state.

### Historical phase T0 — API skeleton

At this historical checkpoint, public types, traits, and function signatures had `todo!()` bodies and
compiled without behavior. No such T0 stub describes the current runtime.
This is itself a reviewable design artifact.

### Historical phase T1 — `testing` module (test-support infrastructure)

Shipped in-crate behind the `testing` feature (also used by downstream consumers' tests):

| pi-agent / pi-ai test util | rust-genai-agent `testing` module |
|---|---|
| `MockAssistantStream extends EventStream` | `ScriptedStream`: channel-backed `Stream<Item = AssistantMessageEvent>` + final-result handle; scripts are preloaded event vecs or a closure driving a sender |
| inline `streamFn` closures returning scripted streams | `MockStreamFn`: `Vec<Script>` or `Fn(request) -> ScriptedStream`; records every `(model, context, options)` call for later assertions |
| `createModel()` | `fixtures::model()` → a mock `ModelSpec`/`ModelIden` |
| `createUsage()` / `createAssistantMessage()` / `createUserMessage()` | `fixtures::{usage, assistant_msg, user_msg, tool_use_msg, tool_result_msg}` builders |
| `fauxText` / `fauxThinking` / `fauxToolCall` content builders | `script::{text, thinking, tool_call}` content + full scripted-response builders (text answer, tool-call turn, abort mid-stream, in-band error) |
| `identityConverter` | `testing::identity_convert_to_llm()` |
| `Type.Object({...})` typebox schemas | `serde_json::json!` schemas |
| `test/utils/calculate.ts`, `get-current-time.ts` | `testing::tools::{calculate_tool(), current_time_tool()}` via the `FnTool` closure adapter |
| `createDeferred()` | `tokio::sync::oneshot` / `Notify` |
| `queueMicrotask(() => stream.push(...))` | scripts are preloaded; for interleaved timing, `tokio::spawn` driving the channel |
| `setTimeout(() => agent.abort(), 30)` | `tokio::time::sleep` + `abort()`; `tokio::time::pause` where determinism matters |
| events collected via `for await` | `EventRecorder`: a sink/listener collecting `Vec<AgentEvent>` behind a `Mutex`, with `assert_sequence(&[EventKind::...])` and payload matchers |

### Historical phase T2 — Tier 1 parity baseline

The current repository contains **52 concrete behavior cases** (no generated `it.each` cases):

| TS file | Rust file | Exact cases |
|---|---|---:|
| `test/agent-loop.test.ts` | `tests/agent_loop.rs` | 21 |
| `test/agent.test.ts` | `tests/agent.rs` | 21 |
| `test/e2e.test.ts` (faux provider) | `tests/e2e_scripted.rs` | 10 |
| **Total** | | **52** |

A machine-readable `tests/parity_manifest.toml` records, for every case: TS source file, exact TS test
name, Rust test path/name, owning milestone, and status (`pending`, `active`, `green`, or documented
`divergence`). `scripts/check_test_parity.py` validates `expected_cases`, pinned upstream coverage,
Rust test existence, and exact ordered equality between `tests/parity/*.toml` and the aggregate; it fails
if a case or status is missing, duplicated, reordered, or silently changed.

Porting conventions:
- Tests are translated **case-by-case, name-by-name** (`it("should emit tool_execution_end in completion
  order but persist tool results in source order")` → `#[tokio::test] async fn
  tool_execution_end_completion_order_results_source_order()`), with a header comment linking the TS source.
- Prefer black-box tests through public APIs. Test support may fabricate rust-genai-agent's own public
  assistant-event stream, but must not duplicate loop or reducer behavior that belongs to production code.
- JS-legacy cases don't translate literally: `Reflect.apply(agentLoop, …, undefined)` /
  `Reflect.construct(Agent, [{}])` omit-streamFn cases become "default stream fn is used when
  `stream_fn` is `None`" tests.
- Every ported behavior test had a substantive, enabled body. T2 intentionally ended with a red
  `cargo test --features testing` run while `--no-run` remained green.
- Implementation then proceeded case-by-case; entries moved from `active` to `green` only with
  passing real behavior. The matrix grew from the original 52 cases to 56 as upstream added
  cases; all 56 are `green`, with no ignored mapped tests.
- Historical reporting separated compilation health from `green / 52 total`; current reporting
  records 56/56 mapped parity, the 112-test M1-M6 checkpoint, the **206/206** agent suite, and
  the **213/213** agent + FFI workspace gate.

### Adapter coverage — landed and planned offline layers

The original `GenaiStreamFn` accumulator plan called for two offline test layers:

1. **Pure fold tests** — the accumulator is a pure state machine `fold(ChatStreamEvent) -> Vec<AssistantMessageEvent>`,
   unit-tested on hand-built `Vec<ChatStreamEvent>` sequences (covers Value::String partial-arg salvage parsing,
   thought-signature attachment, stop-reason mapping, Err→Error conversion). No `ChatStream` construction needed
   (`ChatStream::from_inter_stream` is `pub(crate)` — not fabricable externally).
2. **Yakbak replay tests (planned; not landed)** — rust-genai ships recorded provider cassettes under
   `rust-genai/tests/data/yakbak/` (openai tool-call edge cases, anthropic tool stream + ping heartbeats,
   gemini thinking, openai_resp reasoning/multi-tool-ordering/utf8-chunking, github_copilot, ollama_cloud).
   A future replay-only local server can serve them to a real `genai::Client` and assert the accumulated
   assistant message/event sequence per provider. This replay harness is a follow-up plan, not part of the
   current 206-test agent suite and not a prerequisite for the completed M1 implementation.

### Live-provider smoke (manual)

The `examples/` binaries provide manual live-provider smoke coverage through `GenaiStreamFn`. CI and the
repository tests compile them but never execute a provider request.

## 11. Milestone record

| Milestone | Implementation | Checkpoint status |
|---|---|---|
| **T0** | API skeleton (`todo!()` bodies) | Historical and superseded |
| **T1** | `testing` module | Historical foundation; support remains available behind `testing` |
| **T2** | substantive parity bodies before runtime behavior | Historical red baseline, superseded by the current 56/56 green matrix |
| **M1** | message/assistant/event types, accumulator, `GenaiStreamFn`, validation | Complete; pure fold/protocol tests landed, Yakbak replay remains a separate plan |
| **M2** | loop core: sequential tools, steering/follow-up, error/abort paths | Complete |
| **M3** | parallel tool engine, hooks, truncation guard, terminate | Complete |
| **M4** | `Agent` state/queues/subscribe/abort/wait-for-idle | Complete |
| **M5** | default stream function, package docs, `c01-basic`, `c02-tools`, `c03-steering-abort` | Complete |
| **M6** | feature-gated proxy HTTP/SSE transport | Complete with mock-server, trust-boundary, and resource-cap regression coverage |

Current alignment is **56/56 mapped green**, **206/206** agent-crate all-feature tests green,
and **213/213** for the workspace's agent + FFI gate. The M1-M6 completion checkpoint was
112/112; release-hardening, upstream case additions (52 → 55 → 56), and later parity/runtime
batches account for the increase. Future case or regression additions must update the upstream
pin, parity matrix, documentation, and declared test counts deliberately.
