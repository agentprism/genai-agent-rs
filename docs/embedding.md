# Embedding `rust-genai-agent` in non-Rust hosts

**Status:** design proposal (for review before implementation)
**Audience:** consumers embedding the agent as an in-process orchestration core from
Swift/Kotlin (e.g. a SwiftUI macOS/iOS app) rather than shelling out to a Rust binary.
**Binding mechanism:** [UniFFI](https://mozilla.github.io/uniffi-rs/) — one Rust surface
generates both Swift and Kotlin.

This document proposes a **deliberate, stable embedding surface**: construct from a
data-only config, subscribe one sink, register tools and hooks as trait objects the host
implements, and cancel via an explicit token — with the async runtime owned by the FFI
layer. It is written as a contract to validate before code changes.

---

## 1. Design principles

1. **Additive, never a rewrite.** Every change here *adds* a trait/data-shaped path and
   leaves the existing Rust-native API (closures, `Stream`, `AgentConfig`) intact. The
   core's pi-fidelity is untouched; Rust users notice nothing.
2. **Behavior = trait objects; configuration = data.** Anything the host must *implement*
   is an object-safe trait → a UniFFI **callback interface** → a Swift/Kotlin protocol.
   Anything the host *declares* is a plain data record → a UniFFI **record** (serde-able).
3. **The FFI layer owns the runtime.** The tokio reactor lives in exactly one place; the
   host never manages an executor.
4. **Opt-in and lean.** UniFFI scaffolding is a **separate crate, off by default**, so
   Rust-only and size-sensitive builds pay nothing. Provider backends (`codex`, `auth`,
   `proxy`) stay feature-gated.

---

## 2. What already crosses cleanly (keep as-is)

The crate is ~80% of the way there; these are confirmed in the current code and require no
change:

- **Actor shape** — `prompt()`, `continue_()`, `subscribe`, `wait_for_idle()`, `abort()`.
- **Self-synchronizing events** — every `AgentEvent` carries the full partial message, so
  the host never stitches deltas.
- **Explicit `CancellationToken`** in the public API (`tokio_util::sync::CancellationToken`).
- **Flat `thiserror` enums** (`AgentError`, `ToolHookError`) — map without string flattening.
- **`AgentTool` (`tool.rs`) and `StreamFn` (genai-agentprism `stream_fn.rs`) are already
  object-safe traits**, registerable on the live agent (`set_tools`, `set_stream_fn`). The
  host can implement a tool or a backend today.
- **Messages already derive serde**; proxy/testing off by default.

---

## 3. The gaps, and Layer A — core FFI-shaping (mechanism-agnostic)

These land in `rust-genai-agent` / `genai-agentprism` and benefit *any* binding approach
(and improve Rust ergonomics too). All are additive; the closure APIs remain.

### 3.1 Hook trait mirrors — the central change

Today every hook is `Arc<dyn Fn…>` (`hooks.rs:146-266`), which has no foreign
representation. For each, add an **object-safe trait** plus an `Arc<dyn Trait>` registration
method that wraps the trait into the existing closure internally. Rust keeps closures; hosts
implement the trait.

| Closure hook (current) | Trait mirror (`#[async_trait]`, object-safe) | Register on `Agent` |
|---|---|---|
| `TransformContextHook`<br>`Fn(Vec<AgentMessage>, Cancel) -> Vec<AgentMessage>` | `trait TransformContext { async fn transform(&self, messages: Vec<AgentMessage>, cancel: CancellationToken) -> Vec<AgentMessage>; }` | `set_transform_context_object` |
| `BeforeToolCallHook`<br>`Fn(&mut Ctx, Cancel) -> Option<Result>` | `trait BeforeToolCall { async fn before(&self, ctx: BeforeToolCallContext, cancel) -> BeforeToolCallOutcome; }` | `set_before_tool_call_object` |
| `AfterToolCallHook` | `trait AfterToolCall { async fn after(&self, ctx: AfterToolCallContext, cancel) -> Option<AfterToolCallResult>; }` | `set_after_tool_call_object` |
| `TryBeforeToolCallHook` | `trait TryBeforeToolCall { async fn before(&self, ctx, cancel) -> Result<BeforeToolCallOutcome, ToolHookError>; }` | `set_try_before_tool_call_object` |
| `TryAfterToolCallHook` | `trait TryAfterToolCall { async fn after(&self, ctx, cancel) -> Result<Option<AfterToolCallResult>, ToolHookError>; }` | `set_try_after_tool_call_object` |
| `ShouldStopAfterTurnHook` / `AgentShouldStopAfterTurnHook` | `trait ShouldStopAfterTurn { async fn should_stop(&self, ctx: ShouldStopAfterTurnContext, cancel) -> bool; }` | `set_should_stop_after_turn_object` |
| `PrepareNextTurnHook` / `Agent…WithContextHook` | `trait PrepareNextTurn { async fn prepare(&self, ctx: PrepareNextTurnContext, cancel) -> Option<AgentLoopTurnUpdate>; }` | `set_prepare_next_turn_object` |
| `QueueMessagesHook` (steering & follow-up) | `trait QueueSource { async fn poll(&self) -> Vec<AgentMessage>; }` | `set_steering_source_object` / `set_follow_up_source_object` |
| `OnPayloadHook` / `OnResponseHook` (genai-agentprism) | `trait PayloadInterceptor` / `trait ResponseObserver` | on `GenaiStreamFn` / request (genai-agentprism side) |
| `ConvertToLlm` | `trait ConvertToLlm { fn convert(&self, messages: &[AgentMessage]) -> Vec<ChatMessage>; }` | `set_convert_to_llm_object` (most hosts keep the default) |

**The one real subtlety — the `&mut` borrow bridge.** `BeforeToolCallHook` receives
`&'a mut BeforeToolCallContext` and mutates `ctx.args` in place to rewrite tool arguments
(`hooks.rs:157-164`). A callback interface can't hold a mutable borrow across the language
boundary. The trait mirror therefore takes an **owned** `BeforeToolCallContext` and returns
an **owned** outcome:

```rust
pub struct BeforeToolCallOutcome {
    /// Arguments to execute with (host may have rewritten them). `None` = leave unchanged.
    pub args: Option<Value>,
    /// Block/allow decision, exactly as the closure form returns.
    pub decision: Option<BeforeToolCallResult>,
}
```

The adapter that wraps `Arc<dyn BeforeToolCall>` into the closure writes the returned `args`
back into the `&mut ctx` before returning the decision — so loop semantics are identical,
and the boundary only ever sees owned values.

### 3.2 Sink-based event subscription

The actor's `subscribe` takes a closure (`AgentListener`, `agent.rs:107/966`); an
`AgentEventSink` trait exists but only the lower-level `run_agent_loop` consumes it, and it
uses `&mut self`. Add an `Arc`-friendly sink and wire it into the actor:

```rust
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: AgentEvent);
}
impl Agent {
    pub fn subscribe_sink(&self, sink: Arc<dyn EventSink>) -> Subscription { /* … */ }
}
```

This is the FFI event path → a UniFFI callback interface → a Swift `protocol EventSink`.
The closure `subscribe`/`subscribe_fn` stay for Rust. Because events remain
self-synchronizing, the host renders each event's message directly with no accumulation.

### 3.3 serde on `AgentEvent`

`AgentEvent` derives only `Debug, Clone, PartialEq` (`event.rs:17`) though its payloads are
serde-able. Add `Serialize`/`Deserialize`. This gives:
- a **stable wire/persistence format** for the primary output (logging, replay), and
- an optional **JSON-string sink** fallback (`on_event(json: String)`) for hosts that prefer
  one flat callback over an async callback interface.

### 3.4 `content_index: usize → u32`

The three streaming-delta `content_index: usize` fields (genai-agentprism `assistant.rs:437/444/453`)
become `u32` — fixed-width and FFI-kind. Breaking; ship with the other breaks in one release.

### 3.5 Data-only config + builder

Introduce a declarative, serde-able **`AgentSetup`** holding data only (system prompt, model,
session id, initial messages, thinking budgets/level, `max_retries`, `max_retry_delay_ms`,
`tool_execution`, `transport`, `chat_options`) and an **`AgentBuilder`** that takes an
`AgentSetup` and attaches behavior via the trait registrations above:

```rust
let agent = AgentBuilder::new(setup)          // AgentSetup: a UniFFI record
    .stream_fn(my_backend)                    // Arc<dyn StreamFn>
    .tool(my_tool)                            // Arc<dyn AgentTool>
    .before_tool_call(my_hook)                // Arc<dyn BeforeToolCall>
    .event_sink(my_sink)                      // Arc<dyn EventSink>
    .build();
```

`AgentConfig` (with its closures) stays for Rust. `AgentSetup` is the fully data-representable
config the host constructs declaratively.

---

## 4. Layer B — the `genai-agent-ffi` crate (UniFFI)

A new **opt-in workspace member** (`crate-type = ["staticlib", "cdylib"]`) that is the turnkey
Swift/Kotlin contract. It **owns a multi-thread tokio runtime** created at init; every exported
async method drives on it (UniFFI async), so the host never touches an executor.

Exposed surface:

- **Objects (UniFFI `interface`)**
  - `Agent` — `prompt(text)`, `continue_()`, `subscribe(sink)`, `abort()`, `state()`,
    `wait_for_idle()` (async where the Rust method is).
  - `CancellationToken` — `cancel()`, `is_cancelled()`.
- **Callback interfaces (host implements)** — `EventSink`, `Tool` (`AgentTool`), `StreamFn`,
  and each hook trait from §3.1.
- **Records** — `AgentSetup`, `AgentMessage` + content variants, `AgentEvent`,
  `AgentToolResult`, usage/cost.
- **Enums** — `StopReason`, `ThinkingLevel`, `ToolExecutionMode`, `Transport`, `EventKind`.
- **Errors** — `AgentError`, `ToolHookError` (flat → thrown Swift errors / Kotlin exceptions).

**`serde_json::Value` boundary.** Free-form JSON (tool `args`, result `details`, tool
`schema`) crosses as a JSON **`string`**, parsed by the host's native JSON — avoiding a
recursive UniFFI type. Documented explicitly so the host knows those fields are JSON text.

### Swift, end to end (target shape)

```swift
final class Weather: Tool {                       // implement a tool in Swift
  func spec() -> ToolSpec { /* name, description, JSON-schema string */ }
  func call(_ args: String, _ cancel: CancellationToken) async throws -> AgentToolResult { /* … */ }
}
final class UI: EventSink {                        // one sink, full message per event
  func emit(_ event: AgentEvent) async { /* update SwiftUI @Published state */ }
}

let agent = Agent(setup: AgentSetup(model: "gpt-5.6-sol", systemPrompt: "…", tools: []))
agent.subscribe(sink: UI())
let cancel = agent.signal()                        // explicit token
try await agent.prompt("Plan my week")             // drives on the crate-owned runtime
// cancel.cancel() from a Stop button
```

---

## 5. Type-mapping cheatsheet

| Rust | UniFFI | Swift | Kotlin |
|---|---|---|---|
| `Arc<dyn Trait>` (behavior) | callback interface | `protocol` | `interface` |
| data `struct` (serde) | record | `struct` | `data class` |
| flat `enum` | enum | `enum` | sealed class |
| `thiserror` enum | error | `Error` (throws) | `Exception` |
| `async fn` | async | `async` | `suspend` |
| `CancellationToken` | object | class w/ `cancel()` | class |
| `serde_json::Value` | `string` (JSON) | `String` + `JSONDecoder` | `String` + parser |
| `u32`/`u64` | `u32`/`u64` | `UInt32`/`UInt64` | `UInt`/`ULong` |

---

## 6. Binary size / features

- The FFI crate is separate and opt-in; Rust-only builds exclude it entirely.
- Inside it, keep `codex` / `auth` / `proxy` feature-gated so a mobile build compiles only the
  backends it ships. The minimal footprint is the loop + `GenaiStreamFn`.

---

## 7. Sequencing

1. **A1 — traits & data (no behavior change).** Hook trait mirrors + `subscribe_sink` +
   serde on `AgentEvent` + `usize→u32` + `AgentSetup`/`AgentBuilder`, all closure APIs intact.
   *This alone makes the crate bindable by any mechanism.*
2. **B1 — UniFFI proof.** Minimal `genai-agent-ffi`: construct from `AgentSetup`, one
   `EventSink`, `prompt`, cancel — shipped as a generated Swift package the consumer runs
   end-to-end to validate the shape.
3. **B2 — full surface.** Tools, `StreamFn`, and every hook as callback interfaces; Kotlin
   target; reference Swift/Kotlin samples; embedding docs.

---

## 8. Open questions for the consumer to validate

- **Event delivery:** async callback interface (`emit(event)`) vs. a single flat
  `on_event(json: String)` sink. UniFFI supports both; async is richer, JSON is simplest to
  wire. (We can ship both.)
- **`Value` as JSON string** vs. a structured UniFFI JSON type — string is simplest and
  lossless; confirm it's acceptable for tool args/schema.
- **Runtime model:** a single crate-owned multi-thread runtime (proposed) vs. injecting a
  host-provided executor. The owned runtime is simplest and what we recommend.
- **Targets now:** Swift-only vs. Swift+Kotlin. Structurally identical under UniFFI; only
  affects which bindings we generate.
