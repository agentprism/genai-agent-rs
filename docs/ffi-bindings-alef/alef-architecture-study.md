# Alef architecture study for AgentPrism

Status: source study and adoption proposal, 2026-08-27

## Executive conclusion

Alef works best when a Rust library presents a small, concrete, owned API island inside a richer idiomatic Rust implementation. Liter LLM does exactly that: its foreign surface is mostly data-transfer types, one opaque concrete client, selected concrete operations, a stable error, and two construction helpers. Provider traits, generic builders, Tower services, borrowed internals, raw transports, and most extension seams remain Rust-only.

Its `alef.toml` source list is important, but it is not a strict allowlist. Alef recursively follows public modules and re-exports from parsed roots. The real boundary is the combination of explicit source roots, Rust visibility and re-export structure, `alef(skip)`, and Alef exclusions. The explicit list also drives source hashing, so every binding-driving file should still be named even when recursive discovery would find it.

AgentPrism can fit this model, but it should not copy Liter's generated streaming mechanics unchanged. AgentPrism streams are protocol-bearing, replay-relevant lifecycles with explicit cancellation, terminal validation, acknowledged sinks, and runtime ownership. Those semantics belong in canonical Rust types in `agentprism-runtime-tokio`. Alef should project async pull operations over those types; generated glue should not invent the authoritative queue, cancellation rule, or Tokio runtime.

The recommended public stack is:

```text
foreign package
    -> generated Alef glue
        -> agentprism-native construction facade
            -> TokioRuntimeOwner
            -> TokioModelClient / TokioAssistantStream
            -> TokioAgentFactory / TokioAgentHandle / TokioAgentRun
                -> Models control plane
                    -> Arc<dyn ModelRuntime>
```

This preserves the adopted architecture: `Models` is the provider/auth/catalog control plane, `ModelRuntime` is the narrow execution capability, and the agent depends only on that capability. A native facade assembles the pieces without creating a second event or request hierarchy.

The main prerequisites are also the conclusions of the prior BoltFFI review: a Rust-owned runtime, concrete runtime/client/factory handles, reusable async pull streams, explicit cancellation/drop behavior, `AgentEventEnvelope` at actor fan-out, extractable ID definitions, lossless handling of `i128` money, concrete alternatives to public generic signatures, stable cross-language error kinds, and an explicit policy for ordered/raw JSON. Acknowledged async sinks need separate generator work and should not block a pull-only first release.

Alef is therefore plausible as AgentPrism's projection and packaging system. It should not become the place where AgentPrism's concurrency contract is invented.

## Scope, repositories, and version caveat

This study inspected:

- AgentPrism at `/home/vikash/genai-agent/genai-agent-rs`;
- Liter LLM at `/tmp/claude-1000/-home-vikash-genai-agent-genai-agent-rs/baa530f5-cd0c-4f9c-a0e7-9b22257ad504/scratchpad/liter-llm`;
- Alef at `/tmp/claude-1000/-home-vikash-genai-agent-genai-agent-rs/baa530f5-cd0c-4f9c-a0e7-9b22257ad504/scratchpad/alef`;
- BoltFFI prior art at `/home/vikash/genai-agent/genai-agent-rs-boltffi/docs/boltffi-swift-bindings`.

The inspected Liter commit is `941a92b8a1ad6c520468ddb80e9b5fab046e2b9c`; its `alef.toml` pins Alef `0.68.0`. The separately inspected Alef checkout is commit `eece4b3d3094e8bf55eaadb51de816c4f73f1136`, version `0.71.0`. Extractor and code-generation observations below describe the on-disk 0.71.0 source unless a Liter-generated artifact is named. AgentPrism must select one exact Alef revision and rerun the hard-shape probes against it.

Liter's configuration names 15 generation targets. Its citation text advertises bindings for 14 languages, while user-facing package counts can exclude infrastructure targets such as plain C FFI or JNI. This study does not try to reconcile those counting conventions; it follows the configured and generated artifacts.

No AgentPrism bindings were generated. Claims about current AgentPrism extractability are source-audit findings, not a substitute for a pinned generator prototype.

## Insight 1: Liter exposes a product surface, not its whole Rust architecture

Liter's primary `[[crates]]` declaration in `liter-llm/alef.toml` explicitly names 24 files in the inspected checkout. They fall into a few coherent groups:

| Group | Representative paths | Why it is present |
|---|---|---|
| Stable failure | `crates/liter-llm/src/error.rs` | One canonical error shape for generated APIs |
| Construction facade | `crates/liter-llm/src/bindings.rs` | Scalar/JSON entry into a complex builder graph |
| Request/response graph | `crates/liter-llm/src/types/*.rs` | Owned values accepted and returned by clients |
| Concrete client | `crates/liter-llm/src/client/mod.rs` | Opaque owner plus callable operations |
| Public configuration | `provider/mod.rs`, `provider/custom.rs` | Consumer configuration, not provider implementation |
| Selected controls | `cost/refresh.rs`, `tower/{budget,cache,rate_limit}.rs` | Portable policy records useful to consumers |
| Public root | `crates/liter-llm/src/lib.rs` | Re-exports and canonical paths |

The omitted implementation is at least as significant: provider implementations and wire encoders, generic client/provider traits, raw HTTP exchanges, boxed future/stream aliases, Tower `Service` composition, builders with ecosystem-specific types, auth implementation details, and unsupported subproducts are not part of the common foreign contract.

That split explains “why these files.” They form the transitive value graph and concrete operations of the foreign product. They are not a sampling of source files, and they are not an attempt to translate every public Rust extension point.

### The source list is a reproducibility boundary, not a perfect whitelist

Alef parses Rust syntax with `syn`; the main dispatch is in `alef/src/extract/extractor/mod.rs`. Its module/re-export handling follows public modules and public uses, including workspace-relative modules; see `alef/src/extract/extractor/reexports.rs`. Listing `src/lib.rs` can therefore make a descendant reachable even when the descendant file is not independently listed.

The explicit source set still matters. It gives Alef extraction roots and feeds the source hash in `alef/src/core/hash.rs`. This produces a subtle rule for AgentPrism: minimize what the public native root re-exports, but explicitly list every file that affects generated bindings. Relying only on recursion weakens provenance; relying only on the list weakens surface control.

Liter reinforces the boundary with a large `[crates.exclude]` section. AgentPrism should also use exclusions, but should prefer a smaller purpose-built public root so that a negative list does not become the primary architecture.

### The facade narrows construction, not the data model

`liter-llm/crates/liter-llm/src/bindings.rs` has only two helpers, simplified here:

```rust
create_client(
    api_key: String,
    base_url: Option<String>,
    timeout_secs: Option<u64>,
    max_retries: Option<u32>,
    model_hint: Option<String>,
) -> Result<DefaultClient>

create_client_from_json(json: &str) -> Result<DefaultClient>
```

These helpers hide `ClientConfigBuilder`, `Duration`, file configuration, borrowed conveniences, and provider trait machinery. They return the same `DefaultClient` used by Rust, and they do not redefine chat requests, chunks, errors, or streams.

This is the right meaning of a binding facade: make entry and unavoidable conversions concrete while preserving canonical domain types.

One nuance is that target configuration can customize construction. The generated Swift package uses a configured client-constructor body that builds `ClientConfig`/`DefaultClient`; it need not literally call both facade functions. The facade still records the intended narrow construction surface for backends that use it.

## Insight 2: Alef-friendly Rust has a recognizable shape

Liter's bindable records consistently use owned `String`, `Vec`, `Option`, maps, booleans, and fixed-width numbers. Fields are public when contractual; portable types are non-generic; records and enums normally implement `Clone`, `Serialize`, and `Deserialize`. Async inputs are owned or cloned before work starts. Returned streams are `'static`. `DefaultClient` is a cloneable opaque object whose private fields may contain HTTP clients, config, and `Arc<dyn Provider>` without projecting those internals.

The important Alef 0.71.0 extractor rules, taken from `alef/src/extract`, are:

- public structs with non-lifetime generics are rejected; lifetime-only generics can be accepted;
- public generic enums, functions, traits, aliases, methods, and impls are unsupported surface;
- a one-field tuple/newtype is understood;
- a fieldless non-Serde object can become an opaque handle;
- `#[cfg_attr(alef, alef(skip))]` and hidden items can deliberately remove Rust-only API;
- manual `Serialize`/`Deserialize` impls are detected, not only derives;
- `thiserror::Error` enums become structured error definitions;
- ordinary hand-written `Display + Error` does not provide the same generated error identity;
- native `async fn` and common boxed-future return patterns are recognized;
- boxed stream output shapes are recognized, although portable stream wrappers still depend on adapters.

`alef/src/extract/type_resolver.rs` supports common integer widths through 64 bits, floats, strings, paths, bytes, JSON values, standard collections, duration, and selected wrappers. It often looks through `Box`, `Arc`, `Rc`, `Mutex`, or `RwLock` to the inner type. `i128`/`u128` are not ordinary resolved primitives in the inspected implementation.

Syntactic recognition of `dyn SomeTrait` does not create a useful cross-language vtable contract by itself. Trait objects still need an opaque concrete owner or an explicit, semantically adequate trait bridge.

The sanitizer in `alef/src/cli/pipeline/extract/sanitizer.rs` checks named references against extracted definitions. Unknown named types can otherwise fall back to a string-like lossy form; strict validation reports `lossy_sanitized_surface`. The correct response is to include a lossless value definition, explicitly skip the API, or reshape it—not to accept accidental stringification.

### What remains Rust-only in Liter

Liter's global exclusions cover recurring categories rather than random failures:

- generic LLM, raw, batch, file, and response authoring traits;
- `BoxFuture`/`BoxStream` aliases and concrete transport internals;
- generic builders and advanced configuration objects;
- raw request/response exchanges;
- Tower middleware/service authoring;
- provider and authentication implementation seams;
- functions returning a client builder or other lossy unsupported type;
- response-streaming shapes whose handwritten/newtype representation is not in the common generated product;
- realtime and other incomplete/common-surface exclusions.

The lesson is not that Alef cannot bind a production async library. The lesson is that the portable product API is smaller than the Rust extension API. A broad crate root that re-exports every implementation type would make that distinction fragile.

## Insight 3: a Liter streaming call is an adapter pipeline

The core chat stream lives in `liter-llm/crates/liter-llm/src/client/mod.rs`. In simplified form it consumes an owned `ChatCompletionRequest` and returns a boxed future whose success value is a `'static` boxed stream of `Result<ChatCompletionChunk, LiterLlmError>` items. The client prepares the provider request, applies auth/headers and provider transformation, then decodes SSE or the relevant event stream into typed chunks.

Liter makes this portable with an explicit adapter entry in `alef.toml`:

```toml
[[crates.adapters]]
name = "chat_stream"
pattern = "streaming"
core_path = "chat_stream"
owner_type = "DefaultClient"
item_type = "ChatCompletionChunk"
error_type = "LiterLlmError"
request_type = "liter_llm::ChatCompletionRequest"
```

The metadata supplies a concrete owner, request, item, and error around Rust's erased future/stream machinery. The resulting foreign API is implemented differently per backend.

### End to end: Rust into Swift

The generated Rust bridge is in `liter-llm/packages/swift/rust/src/lib.rs`; the generated Swift API is in `liter-llm/packages/swift/Sources/LiterLlm/LiterLlm.swift`; `liter-llm/e2e/swift_e2e/Tests/LiterLlmE2ETests/StreamingTests.swift` consumes it with `for try await`.

The call path is:

1. Swift calls generated `DefaultClient.chatStream(request)`.
2. Generated Swift starts a detached task and invokes the generated Rust start function.
3. Rust clones the opaque client/request and uses Alef's generated process-wide Tokio handle to wait for `client.chat_stream(...)` to open.
4. The opened stream is stored in a mutex-protected `DefaultClientChatStreamStreamHandle` together with a Tokio handle.
5. A detached Swift loop repeatedly invokes the handle's blocking `next` bridge.
6. Rust waits for `StreamExt::next`, serializes an item to JSON, and returns it across the bridge.
7. Swift decodes `ChatCompletionChunk` and yields it into `AsyncThrowingStream`.
8. An empty result represents EOF; an error string finishes the Swift stream with an error.
9. `onTermination` cancels the Swift task; releasing the opaque handle ultimately drops the Rust stream.

This is an idiomatic Swift consumer API, but runtime ownership, serialization, cancellation, terminal signaling, and error projection are choices made by the generated adapter. Some Liter Swift E2E comments also show that backend error-identity assertions are not uniformly strong.

The Swift package itself combines generated Rust static/CDylib output, bridge-generated C/Swift sources, an in-tree `Package.swift`, and binary artifact packaging for publication. It is a useful template for the shape of an AgentPrism Swift deliverable.

### End to end: Rust into Node

The Node bridge in `liter-llm/crates/liter-llm-node/src/lib.rs` takes a different path:

1. TypeScript awaits `client.chatStream(request)` and receives an async iterator.
2. Generated Rust clones the client, converts the owned request, and creates a Tokio MPSC channel of capacity 32.
3. A spawned task awaits the core stream-opening future.
4. It forwards each chunk or item error into the bounded channel.
5. The N-API iterator's `next()` locks the receiver and awaits one item.
6. TypeScript consumes items with `for await`.
7. Dropping the iterator closes the receiver; a later failed send stops the producer and drops the source stream.

Sends are awaited, so a live slow consumer applies backpressure instead of losing chunks. Even so, this is an additional queue and scheduling layer absent from the source stream.

| Property | Swift wrapper | Node wrapper |
|---|---|---|
| Host abstraction | `AsyncThrowingStream` | async iterator |
| Rust polling | global Tokio handle plus blocking bridge | spawned Tokio task |
| Extra MPSC | none visible in inspected wrapper | bounded channel, capacity 32 |
| Item conversion | JSON then Swift decode | generated N-API conversion |
| EOF | stream-handle sentinel | closed receiver |
| Consumer drop | task cancellation plus handle drop | receiver close then send failure |

Alef successfully supplies language idioms, but those idioms have observable effects. “Streaming” does not by itself prove equivalent buffering, cancellation, error identity, or executor lifetime.

## Insight 4: per-target exclusions are honest capability boundaries

Liter has both global and target-specific exclusions. The inspected configuration excludes Linux musl package variants for Node; `ensure_crypto_provider` for PHP, with an explicit `ext-php-rs`/`Self` resolution note; selected budget/cache/rate-limit and token-count functions for WASM; token functions for JNI; `all_providers` for Swift; and selected policy/config/token functions for Kotlin Android. It also selects `wasm-http` and particular Android ABIs.

Only some reasons are documented adjacent to the entries. Runtime, dependency, platform, or backend limitations are plausible for others, but cannot be asserted from source. The defensible lesson is that target exclusions are capability declarations. AgentPrism may use them for genuinely unavailable platform facilities, but must never omit individual terminal/replay event variants and still call the stream lossless. If a backend cannot carry the protocol, defer that whole runtime surface.

## Insight 5: AgentPrism's portable boundary is a native runtime, not bare `Agent`

The current construction path is approximately:

```text
Models / Arc<dyn ModelRuntime>
    -> Agent::new(...)
        -> TokioAgentHandle::new(...)
            -> TokioAgentRun
```

Direct inference is approximately `Models::stream_simple(...) -> AssistantStream`. These are sound Rust dependency seams, but not yet a self-contained foreign consumer API.

The BoltFFI inventory and adopted owner review in the sibling worktree—`docs/boltffi-swift-bindings/api-inventory.md` and `docs/boltffi-swift-bindings/owner-review-2026-08-26.md`—established constraints that any generator must respect:

- the boundary is a concrete Tokio actor, not bare `Agent`;
- Rust owns the Tokio runtime used by exported objects;
- authoritative streams are lossless async-pull objects over Rust-owned bounded queues;
- task cancellation is distinct from run cancellation;
- outcome observation does not require draining events;
- active-run/stream drop cancels work and releases runtime leases;
- a ready-value/cancel handoff must not consume or lose the value accidentally;
- concurrent pulls are rejected deterministically;
- clean EOF, missing required terminal event, cancellation, and in-band provider failure remain distinguishable;
- event sinks are acknowledged barriers, not fire-and-forget notifications;
- sink-only operation must not allocate an unused pull queue;
- `AgentEventEnvelope` is promoted before actor fan-out;
- `Models`, execution capability, runtime ownership, and agent assembly remain separate responsibilities.

Those are product semantics, not BoltFFI-specific workarounds. Alef adoption should implement the same canonical Rust boundary.

### Recommended canonical types

| Type | Responsibility | Key foreign-facing operations |
|---|---|---|
| `TokioRuntimeOwner` | Own executor and leases, independent of models | construct; lifecycle/status as needed |
| `TokioModelClient` | Hold/share `Models` plus a runtime lease | start direct model request |
| `TokioAssistantStream` | Own direct-model stream lifecycle | `next(&self)`, `cancel()`, terminal status |
| `TokioAgentFactory` | Hold runtime, models, and selected tool registry | create configured agent handle |
| `TokioAgentHandle` | Own actor and start/control prompts | prompt with concrete owned input; cancel/control |
| `TokioAgentRun` | Own one run's event/completion views | `next_event(&self)`, `outcome(&self)`, `cancel()` |

`TokioAgentRun::next_event` should return `Result<Option<AgentEventEnvelope>, TokioAgentError>`, not the current bare `AgentEvent` through a mutable receiver. `outcome` should be reusable rather than consuming the run. Concurrent polls, terminal validation, explicit cancellation, active-handle drop, and runtime-lease release need canonical behavior.

`TokioAssistantStream` should similarly wrap `AssistantStream` as an owned pull lifecycle. A cancelled host task should not silently consume a ready item; explicit `cancel()` should cancel model work; drop should release an established stream and runtime lease.

These semantic types belong in `agentprism-runtime-tokio`, not in generated glue. The current mostly monolithic `crates/agentprism-runtime-tokio/src/lib.rs` will likely benefit from splitting runtime owner, model client, agent actor/run, factory, and error into modules so portable methods are not intermingled with generic/raw Rust accessors.

## Insight 6: one native assembly root, one small facade

The prior design's proposed `agentprism-native` crate remains the best Alef root, although it does not exist in the current workspace. It should be a legitimate Rust assembly crate depending on `agentprism-ai`, `agentprism-core`, `agentprism-runtime-tokio`, eventual native transport, `agentprism-providers-all` or an explicit provider set, and persistent credentials where enabled.

It would re-export only the portable consumer graph under one `core_import`, giving generated packages one native library to link and Alef one canonical path for cross-crate types. Provider implementations and Rust authoring seams remain available from their existing crates without being re-exported through this product root.

`agentprism-native/src/bindings.rs` should contain only concrete entry/conversion helpers:

- construct `TokioRuntimeOwner` from simple owned settings;
- construct `Models`/`TokioModelClient` from scalar settings or validated JSON;
- construct `TokioAgentFactory` from runtime, models, and concrete defaults;
- provide no-tools or registered-built-in-tools agent creation initially;
- provide `Vec<AgentRecord>` overloads where canonical Rust uses `IntoIterator`;
- provide validated exact JSON string/byte accessors for raw ordered payloads;
- provide a decimal-string bridge for 128-bit money until Alef has a proven lossless mapping.

It should not contain duplicate requests/events/envelopes, another stream queue, a hidden runtime unrelated to `TokioRuntimeOwner`, an OpenAI-only client bypassing `Models`, an event drainer, or a synchronous callback substituted for an acknowledged async sink.

If a direct `agentprism-ai` package is later desired without the agent runtime, it may justify a separate small construction facade. It cannot assemble provider implementations from within `agentprism-ai` without reversing dependency direction, so the native assembly root is the right first product.

## Insight 7: enumerate the transitive portable graph, not implementation modules

A rough post-refactor source closure would be about two dozen binding-driving files. The exact list should be generated only after the canonical boundary exists.

| Layer | Rough files to enumerate |
|---|---|
| Native root | `agentprism-native/src/{lib,config,models_factory,bindings}.rs` |
| Tokio boundary | proposed `runtime_owner.rs`, `model_client.rs`, `assistant_stream.rs`, `agent_actor.rs`, `agent_run.rs`, `factory.rs`, `error.rs`, and a narrow `lib.rs` |
| AI identities/values | `ids.rs`, `usage.rs`, `messages.rs`, `replay.rs`, `handoff.rs`, `deferred.rs` |
| AI request/event surface | selected concrete definitions from `options.rs`, `model.rs`, `runtime.rs`, `streaming.rs`, `cancellation.rs` |
| Optional control-plane values | selected catalog/auth/image records required by the consumer surface |
| Core values | `events.rs`, `state.rs`, `control.rs`, `error.rs`, value portion of `tools.rs`, proposed `input.rs` |

`agentprism-core/src/run.rs` currently mixes useful input values with runtime/generic implementation. Splitting `AgentInput`, `PromptText`, and prompt-image values into a portable `input.rs` would create a clearer boundary than a large exclusion list for the whole run module.

Do not enumerate provider/API implementations, wire encoders, middleware authoring, scripted runtimes, local non-`Send` twins, raw environment handles, or borrowed scheduler/policy internals. Session and harness should be a later explicitly designed surface, not an accidental consequence of recursive re-exports.

Because Alef follows public modules, keep the native root narrow. Because hashing uses explicit sources, list every file that affects the binding even if the extractor could reach it recursively. CI should also snapshot extracted IR/public API so source hashes are not the only leak detector.

## Insight 8: current extractability divides into clear categories

### Clean or close to clean

Owned records made from strings, vectors, options, maps, booleans, and up-to-64-bit numbers fit Alef well. `Timestamp` is an explicit `i64` newtype; most `Usage` counters are straightforward; many named `AssistantEvent` variants and `AgentEventEnvelope` match normal Serde data; opaque cloneable handles with private fields are a supported pattern; concrete async `Result<T, E>` methods are a natural adapter input.

`CancellationToken` can be an opaque class exposing `new`, `cancel`, `is_cancelled`, `check`, and possibly `child`; its borrowed future-returning wait primitive should stay Rust-only. “Close” still requires backend compilation: for example, `Arc<[ToolCallId]>` may sanitize to a vector shape, but conversion and ownership need a generated probe.

### Macro-generated IDs are invisible to syntax-only extraction

`agentprism-ai/src/ids.rs` uses `macro_rules! string_id` for provider, model, API, replay, extension, message, content block, tool call, replay item, run, and auth challenge IDs. Alef's extractor parses syntax but does not expand ordinary Rust macros. The `Item::Macro` is not an expanded struct definition, so these IDs are not reliably extractable even though `rustc` sees them.

Credible fixes are to spell the canonical structs explicitly, generate an explicit canonical Rust source before Alef, or add and pin macro-expansion support in Alef. Mapping each identity to an unrelated facade `String` would weaken the type system and create a duplicate DTO graph; explicit definitions are the simplest current option.

### Public generics need concrete consumer forms or skips

`ThinkingLevelMap<T>`, `LevelSupport<T>`, `ReasoningLevelResolution<T>`, generic builders, `stream_api<A>`, `impl Into`, and `impl IntoIterator` signatures are not portable Alef surface. Public generics can be critical extraction failures rather than warnings.

Keep them available in Rust but outside the native re-export root, mark appropriate methods `#[cfg_attr(alef, alef(skip))]`, or add concrete overloads such as `Vec<AgentRecord>`. Construction-only concrete overloads can live in `bindings.rs`; meaningful runtime operations should preferably become canonical methods.

### Trait objects remain behind concrete owners

`ModelRuntime`, provider/API traits, credential/auth callbacks, tools, `AgentEventSink`, and boxed future/stream aliases are Rust capability seams. Most should be excluded while concrete handles invoke them internally. This preserves `Models` as the control plane and `ModelRuntime` as the agent's narrow capability rather than teaching generated code about providers.

Foreign-defined tools and event sinks are separate callback/trait-bridge products. They should be added only after threading, acknowledgment, reentrancy, error, and cancellation semantics pass conformance tests.

### `i128` money must not be downcast

`Cost` and `MoneyRate` use 128-bit-scale integers to preserve precision. The inspected Alef resolver does not offer an ordinary `i128` primitive mapping. Downcasting or passing through floating point would violate losslessness.

Options are an Alef/backend extension for signed 128-bit values, a canonical validated decimal-string value, or high/low words in a stable value object. Decimal text is broadly portable across Swift and JavaScript, but the decision must be canonical and tested at boundaries and extrema.

### Ordered and raw JSON require a Rust-owned lane

AgentPrism's JSON has stronger requirements than ordinary DTO projection. Ordered objects participate in wire fidelity; `RawValue` preserves already serialized content; replay/extensions must retain unknown information; UTF-16-compatible key ordering and unusual strings can matter.

Host dictionaries can reorder keys, normalize numbers, reject unusual strings, or reserialize differently. Normal semantic fields should remain generated records, while exact wire/raw payloads cross as validated Rust-owned strings or bytes. No package should claim request-body parity after host JSON decode/re-encode.

`SimpleGenerationOptions.telemetry_context`, erased API patches, and raw-value fields need explicit skip, opaque handle, or serialized accessor treatment. A broad JSON-value projection is not automatically faithful.

### Manual Serde is visible, but backend shape still needs a corpus test

`Message`, replay targets, deferred submissions, records, and events include manual or mixed Serde. Alef 0.71.0 detects manual `Serialize` and `Deserialize` implementations, so manual Serde is not inherently invisible. It still does not prove that every tagged/newtype variant is emitted correctly by each backend. The canonical event/replay fixture corpus should become a generated-binding golden suite.

Alef does not appear to turn Rust's `#[non_exhaustive]` into a complete foreign evolution contract. Generated targets know current variants. Adding a variant requires regeneration, API review, and forward-compatibility testing rather than reliance on Rust compiler behavior.

### Errors need stable identity

`AgentError`, `ControlError`, `TokioAgentError`, and `RequestStartError` use hand-written errors and several are non-exhaustive. Liter's `thiserror` enum gives Alef more structured error information. AgentPrism should either adopt structured `thiserror` where that improves canonical Rust, or expose stable error-kind records plus message/source metadata from concrete runtime handles.

Foreign code must distinguish cancellation, concurrent pull, missing required terminal event, request-start/provider failures, and ordinary validated EOF. A flattened error string is insufficient.

## Insight 9: use async pull, not Alef's generic stream adapter, for authoritative streams

Liter proves the generic streaming adapter can make a chunk stream pleasant. It also shows why it should not own AgentPrism's semantics: Node adds a capacity-32 queue; Swift uses a generated global runtime and blocking bridge; host task cancellation differs from run cancellation; drop and error projection are backend-specific; the adapter does not validate required AgentPrism terminal events.

Expose `TokioAssistantStream::next` and `TokioAgentRun::next_event` as ordinary concrete async methods. Let Alef generate async method bridges, and let host packages layer `AsyncSequence`/async iterators over repeated pulls. The loop can forward explicit cancellation without introducing a second Rust queue.

This leaves one canonical implementation of the important rules: no loss beyond capacity, exact ordering, one item per pull, deterministic rejection of concurrent pulls, no consumption when a waiting host task is cancelled, explicit cancellation of underlying work, active-handle drop cleanup, outcome without drain, and missing-terminal detection.

## Insight 10: acknowledged sinks expose a current generator gap

`AgentEventSink` returns a future and the actor awaits it. Completion is an ordering barrier, not a notification hint. The older Alef `callback_bridge` implementation in `alef/src/adapters/callback_bridge.rs` is not a sound basis for that contract: inspected source labels parts early/unimplemented, contains backend placeholders, and lacks complete target support.

Alef's newer trait-bridge machinery is broader, but the inspected Swift generator in `alef/src/backends/swift/gen_bindings/trait_bridge.rs` deliberately invokes the inbound Swift shim synchronously even while implementing an async Rust trait method. That is not evidence of a genuinely acknowledged async Swift sink.

The first Alef surface should therefore be pull-only. It must not replace the sink with fire-and-forget callbacks. Sink support can be added after Alef or a target bridge proves that callbacks are awaited, slow sinks lose nothing, sink-only mode has no unused pull queue, cancellation inside callbacks cannot deadlock, reentrancy/thread rules are documented, callback failures have stable outcomes, and drop releases tasks/runtime leases.

## Insight 11: start with a narrow, explicit Alef configuration

An initial configuration could have this shape after the canonical refactor. It is illustrative, not validated final syntax:

```toml
[workspace]
alef_version = "<exact pinned version>"
languages = ["swift", "node"]

[[crates]]
name = "agentprism-native"
core_import = "agentprism_native"
version_from = "crates/agentprism-native/Cargo.toml"
workspace_root = "."
features = ["native"]
auto_path_mappings = true
sources = [
  "crates/agentprism-native/src/lib.rs",
  "crates/agentprism-native/src/config.rs",
  "crates/agentprism-native/src/models_factory.rs",
  "crates/agentprism-native/src/bindings.rs",
  "crates/agentprism-runtime-tokio/src/runtime_owner.rs",
  "crates/agentprism-runtime-tokio/src/model_client.rs",
  "crates/agentprism-runtime-tokio/src/assistant_stream.rs",
  "crates/agentprism-runtime-tokio/src/agent_run.rs",
  "crates/agentprism-runtime-tokio/src/factory.rs",
  "crates/agentprism-runtime-tokio/src/error.rs",
  "crates/agentprism-ai/src/ids.rs",
  "crates/agentprism-ai/src/usage.rs",
  "crates/agentprism-ai/src/messages.rs",
  "crates/agentprism-ai/src/replay.rs",
  "crates/agentprism-ai/src/handoff.rs",
  "crates/agentprism-ai/src/deferred.rs",
  "crates/agentprism-ai/src/streaming.rs",
  "crates/agentprism-core/src/events.rs",
  "crates/agentprism-core/src/state.rs",
  "crates/agentprism-core/src/control.rs",
  "crates/agentprism-core/src/input.rs",
]

[crates.swift]
package_name = "AgentPrism"

[crates.node]
package_name = "@agentprism/native"

[crates.exclude]
types = [
  "ModelRuntime",
  "AssistantStream",
  "SendBoxFuture",
  "AgentEventSink",
  "ModelsBuilder",
]
```

The production exclusion list should name exact types/methods with nearby reason comments; the sample only communicates categories. Async adapter declarations will depend on the pinned Alef syntax and on whether canonical methods are native `async fn` or boxed futures.

Alef's raw extraction pipeline groups workspace source crates and supports type-only roots/path mappings. A native root that re-exports the portable graph should make `auto_path_mappings` simple. If canonical paths cannot be unambiguously rooted there, disable automatic mapping and configure explicit mappings/dependencies rather than accepting sanitized aliases.

## Insight 12: adoption is a release pipeline, not one generator command

### Phase 0: a disposable hard-shape probe

Before canonical changes, pin Alef and create a small cross-crate fixture with exact representatives of macro IDs, `i128`, all event enum forms, manual Serde/newtypes, `RawValue`, ordered JSON, opaque handles, `next(&self) -> Result<Option<T>, E>`, cancelled pending pull/ready-value races, stable errors, Swift 6 `Sendable`, and multi-crate path mapping. Include an acknowledged sink only if sinks are a first-release requirement.

Generate, compile, and run Swift and Node. Extraction success alone is insufficient because their stream/runtime bridges differ materially.

### Phase 1: land the canonical Rust boundary

Implement `TokioRuntimeOwner`, model client, assistant pull stream, factory, envelope-delivering agent run, explicit cancellation, reusable outcome, terminal validation, and drop/lease semantics in their owning crates. Reshape IDs, money, raw JSON access, errors, and generic convenience only where the result is a sound canonical Rust API. Keep other extension seams out of the native root or mark them skipped.

Create `agentprism-native` as a normal assembly crate and add the small construction/conversion facade.

### Phase 2: generate, scaffold, and package

Following Liter's workflow: run Alef scaffold once; generate with formatting; generate target docs/readmes; run Alef verification; and require clean regeneration in CI. Generated files carry provenance hashes and are overwritten, so handwritten target ergonomics must live only in supported scaffold/user-owned regions.

Swift output should resemble Liter's package architecture: generated Rust static library/CDylib, bridge-generated C and Swift sources, an in-tree SwiftPM package, and per-platform binary artifacts or an XCFramework/artifact bundle for release. A small Swift `AsyncSequence` convenience can loop over canonical pull/cancel operations.

Node output should contain a generated N-API crate, declarations/package entry points, and per-platform native addon packages. A TypeScript async iterator can similarly loop over the canonical pull handle without adding another Rust event queue.

Only after those targets pass should Python, Ruby, PHP, C FFI, Go, Java/JNI, C#, Elixir, WASM, Zig, Dart, or Kotlin Android be added. Each target expands the semantic and artifact matrix; none should be enabled merely because Alef can scaffold it.

## Verification: prove the foreign projection preserves the Rust contract

Alef's own source/hash verification is necessary but not sufficient. AgentPrism needs boundary conformance derived from the architecture gates and prior owner review.

### Extraction and public API

- clean generation from a fresh checkout produces no diff;
- every binding-driving source participates in provenance;
- extracted IR/public-surface snapshots change only under review;
- no unknown named type is lossily sanitized;
- provider/API traits and raw accessors do not leak through the native root;
- target docs contain the intended consumer API and omit Rust authoring seams.

### Value fidelity

- every assistant event and agent envelope preserves variant, sequence, run ID, payload, and extensions;
- replay records/targets survive Rust-to-target-to-Rust fixtures where bidirectional conversion is offered;
- money round-trips at extrema without precision loss;
- exact raw/wire JSON never takes a host decode/re-encode path;
- invalid IDs, decimals, and raw JSON return stable error kinds;
- new/non-exhaustive variants trigger regeneration/API review rather than silent fallback.

### Stream and run lifecycle

- more events than queue capacity arrive exactly once and in order;
- slow consumers apply the intended backpressure;
- one pull returns at most one item and concurrent pulls are rejected deterministically;
- cancelling a waiting host task does not consume a ready item;
- explicit `cancel()` ends underlying work;
- dropping an active stream/run cancels work and releases its runtime lease;
- ready-value/drop handoff does not leak an actor;
- normal runs deliver `RunFinished` before validated EOF;
- EOF without the required terminal becomes `MissingRunFinished` or a stable equivalent;
- provider failures remain in-band when the canonical protocol says they do;
- `outcome()` completes without draining or reordering events;
- runtime-owner lifecycle is safe across dependent-handle drops.

### Sinks, when a target eventually supports them

- a held callback future is an actual barrier;
- slow sink delivery remains ordered and lossless beyond capacity;
- sink-only creates no unused pull queue;
- cancellation inside a callback cannot deadlock;
- callback errors and reentrancy follow documented stable rules;
- drop releases tasks and leases.

### Packaging

- Swift passes every published Apple slice and Swift 6 concurrency checks;
- Node generator/iterator drop releases Rust resources;
- release binaries do not require a consumer Rust toolchain;
- binary/package versions correspond to the generated-source hash;
- target exclusions have API tests, not only TOML comments.

The existing replay, wire, agent, and session gates remain authoritative for Rust behavior. Binding tests add proof that each foreign projection preserves rather than redefines those contracts.

## Recommended first release boundary

The first Alef-backed product should be intentionally narrower than the workspace:

1. Swift and Node;
2. explicit native runtime ownership;
3. native model construction through `Models`;
4. direct model streaming through `TokioAssistantStream` async pull;
5. agent prompting through `TokioAgentRun` async pull of `AgentEventEnvelope`;
6. explicit cancellation and reusable outcome observation;
7. no foreign-defined tools initially;
8. no acknowledged sink until its bridge passes the barrier suite;
9. no raw provider/API traits;
10. no session/harness bindings until their consumer contracts are separately designed.

This slice is large enough to test Alef against the hard requirements—async execution, lossless streaming, event fidelity, cancellation, and runtime ownership—without making every Rust extension seam a permanent cross-language promise.

## Decisions to preserve

- Do not make `Agent` depend directly on `Models` or provider crates.
- Do not collapse the lossless assistant event stream to text/tool deltas.
- Do not let generated glue own the only Tokio runtime.
- Do not equate host-task cancellation with AgentPrism cancellation.
- Do not add an unbounded or lossy authoritative queue.
- Do not replace acknowledged sinks with notifications.
- Do not reserialize exact ordered/raw JSON through host dictionaries.
- Do not downcast 128-bit money.
- Do not create a duplicate FFI event/type hierarchy.
- Do not treat Alef's source list as an API allowlist.
- Do not expose a target until its lifecycle/fidelity suite passes.

## Unknowns that require a prototype

- Which exact Alef revision should become AgentPrism's generator baseline.
- Exact output for AgentPrism's mixed manual-Serde event enums on Swift and Node.
- Whether `Arc<[T]>`, `RawValue`, and every nested newtype compile losslessly in both targets.
- The best native/generated mapping for signed 128-bit money.
- Whether Alef runtime hooks can use an AgentPrism-owned runtime without creating a conflicting global runtime.
- Whether Alef can be enhanced to implement genuinely acknowledged async sinks in priority targets.
- The minimum Swift/Node release artifact matrix.
- Which auth/credential flows belong in the first foreign product.
- Whether foreign-defined tools are a first-class release need or a later trait-bridge milestone.

Some Liter per-target exclusions have no adjacent rationale; this study reports them without inventing one. The Liter generated code was inspected as checked in, not freshly regenerated. The Alef source checkout is newer than Liter's pin. Those limitations make the pinned disposable probe the next required technical step.

## Final assessment

Liter succeeds with Alef because one concrete client and its owned value graph are the center of the foreign product, while richer Rust extension seams remain behind the boundary. Its facade solves construction, exclusions protect the product shape, and adapters supply language idioms.

AgentPrism should adopt the same structural discipline but not Liter's stream mechanics. Its portable center should be the concrete Tokio runtime boundary backed by `Models` and `ModelRuntime`, with Rust-owned async pull lifecycles carrying complete replay-aware events.

If that canonical boundary lands first, Alef can remove substantial per-language binding work and standardize Swift, Node, and later packaging. If Alef is introduced first, the generator will either encounter unsupported Rust seams or quietly choose queue, cancellation, runtime, error, and JSON semantics that belong to AgentPrism.

The adoption criterion is therefore:

> Alef is suitable when it projects AgentPrism's canonical native contract; it is not suitable as the place where that contract is invented.
