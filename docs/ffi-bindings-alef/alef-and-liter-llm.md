# Alef and liter-llm: architecture of a source-driven polyglot boundary

## Scope and version note

This study examines two specific checkouts: Alef as the binding generator and liter-llm as its production consumer:

- Alef: <https://github.com/xberg-io/alef>
- liter-llm: <https://github.com/xberg-io/liter-llm>

Repository paths are written as `alef/...` and `liter-llm/...` for readability.

The supplied Alef checkout is version 0.71.0 (`alef/Cargo.toml`), while liter-llm pins Alef 0.68.0 (`liter-llm/alef.toml`). That
distinction matters. The architectural model is continuous across the two, but some generated-file stamps and verification details
in the 0.71.0 source postdate liter-llm's checked-in 0.68.0 artifacts. Where this study describes “current Alef,” it means the
supplied 0.71.0 checkout; where it traces generated code, it means the files actually checked into the supplied liter-llm checkout.

## Executive view

Alef is a source-driven, multi-backend binding generator. It reads ordinary Rust source with `syn`, builds a language-neutral API
model, validates that model, and asks a target backend to emit native wrappers and package material. It also owns the surrounding
generated surface: type stubs, manifests, loaders, reference documentation, runnable end-to-end projects, freshness markers, and
packaging inputs.

The important word is *source-driven*. Alef does not compile the crate and ask rustc for its final public API, and it does not
require a parallel interface definition language. The Rust source is the schema, supplemented by `alef.toml` where source syntax
alone cannot express cross-language intent.

liter-llm succeeds with this model by maintaining two deliberately different surfaces:

- The Rust-native engine uses traits, trait objects, erased futures and streams, provider-specific transports, Tower services, and
  internal state.
- The binding surface uses owned, non-generic data-transfer types, one opaque concrete client, scalar construction functions, and
  explicit async and streaming adapter declarations.

Alef is therefore not exporting “the whole Rust crate.” It is projecting a curated portable façade over the crate. liter-llm's
architecture makes that façade small enough to validate and generate as 13 host-language package families, with C FFI and JNI as
additional infrastructure targets. Individual release gates can still lag that configured surface.

## Insight 1: configured sources are discovery roots, not an API allowlist

Alef starts extraction from each configured `sources` entry. For every root it:

1. Reads the file and parses it as a `syn::File`.
2. Derives its module position relative to the crate source directory.
3. Walks items and recursively resolves modules.
4. Deduplicates visited files.
5. Produces raw functions, types, methods, traits, and documentation in an intermediate representation.

The implementation is in `alef/src/extract/extractor/mod.rs`. Because module traversal is recursive, a listed `lib.rs` can lead Alef
into many files that were not themselves listed. Conversely, listing a file does not make all its contents bindable: visibility,
re-export rules, source annotations, configuration filters, and validation still apply.

That makes a source entry three things at once:

- a discovery seed;
- a module-path and provenance input;
- in current Alef, an explicit input to freshness hashing.

The last point is easy to miss. Current `ResolvedCrateConfig::source_hash_paths()` hashes the direct `sources` and
`source_crates[*].sources` entries, sorted and deduplicated. It does not add every module file found recursively. A project that
cares about a discovered file invalidating generated output should enumerate it explicitly, even if a listed `lib.rs` would discover
it anyway. See `alef/src/core/config/resolved/mod.rs` and `alef/src/core/hash.rs`.

### Visibility is the first surface filter

The extractor admits public structs, enums, free functions, type aliases, and traits. For inherent implementations, it admits public
methods. Struct fields are included only when the fields themselves are public; a type with private state consequently becomes an
opaque boundary instead of leaking its layout. Functions and methods whose names begin with `_` are skipped, as are `#[cfg(test)]`
items.

Public does not mean universally translatable. The extractor records a diagnostic for unsupported generics on structs, enums,
functions, traits, aliases, and methods. Lifetime-only struct generics are tolerated, but general type parameters are not a portable
API escape hatch. Trait-object fields are also excluded from the data surface. These checks are visible throughout
`alef/src/extract/extractor/`, especially `items/`, `impls/`, and `helpers/attributes.rs`.

Alef recognizes a practical vocabulary of Rust types: primitives, strings, paths, bytes, JSON values, options, vectors, maps, sets,
results, common smart pointers, locks, durations, secrets, and selected borrowed or array forms. Wrappers such as `Box`, `Arc`, and
references are generally looked through in the API model. Unknown types can be sanitized while the model is assembled, but current
validation rejects a lossy sanitized public surface rather than silently pretending it is safe. The resolver and guard are in
`alef/src/extract/type_resolver.rs`, `alef/src/cli/pipeline/extract/sanitizer.rs`, and `alef/src/core/validation/`.

### Re-exports can define the public path

Module and re-export handling is not a simple file walk. Alef recognizes local `pub use` forms using `crate`, `self`, and `super`,
including named and glob re-exports. A public re-export can make items in a private module discoverable, and the re-exported path
becomes the Rust path recorded for generation. A glob re-export can flatten the module path; a named re-export filters a private
module to the selected item.

Alef can also follow a re-export into another workspace crate when the target source can be resolved from path dependencies,
workspace dependencies, or its workspace layout heuristics. It deliberately skips local paths in that external resolver. This is
useful, but it is not rustc's complete name resolution or Cargo feature resolver. The ground truth is
`alef/src/extract/extractor/reexports.rs`.

The practical consequence is that bindability is determined by the combined graph of configured roots, syntactically resolvable
modules, public visibility, and public re-exports. A source file's presence in `alef.toml` alone proves none of those later
conditions.

### Alef models source, not expanded compiler output

The parser sees ordinary syntax and selected attributes; it does not run macro expansion. Macro invocations that would synthesize
API items are not an equivalent substitute for visible Rust declarations. Conditional compilation is also modeled syntactically.
Alef understands common `feature`, `any`, `all`, and `not` forms well enough to strip fields and items for a configured feature set,
but this remains a source analysis rather than a rustc query.

Native `async fn` is recognized. The extractor also has syntactic recognition for return shapes such as `BoxFuture`, `BoxStream`,
and `Pin<Box<dyn Future<Output = ...>>>`; see `alef/src/extract/extractor/functions/returns.rs`. Recognition does not imply that
every backend can expose the raw Rust shape. Adapter metadata and backend capabilities decide how a portable async or streaming API
is ultimately made.

## Insight 2: the final surface is a staged policy decision

After raw extraction, the CLI pipeline performs a sequence of policy and normalization passes. In current Alef the significant steps
are:

1. extract from the configured roots and any configured source crates;
2. add declared opaque types and propagate the selected feature model;
3. apply source-level and global exclusions;
4. resolve qualified names, sanitize, map paths, deduplicate, and normalize;
5. add configured services and extensions;
6. mark methods handled by async or streaming adapters;
7. strip configured methods and apply final validation.

The orchestrator is `alef/src/cli/pipeline/extract.rs`.

There are several distinct selection mechanisms, and they should not be collapsed into the word “exclude”:

- `#[cfg_attr(alef, alef(skip))]` and `#[alef(skip)]` are source-owned escape hatches. `#[doc(hidden)]` is also binding-excluded.
- Global `exclude.types`, `exclude.functions`, `exclude.methods`, and `exclude.fields` remove named elements for every target.
- Global `include.types` and `include.functions` act as an allowlist. Included types pull in transitive named dependencies found in
  fields and method or function signatures. Unmatched includes fail rather than silently doing nothing; unmatched excludes are
  warnings.
- Per-language `exclude_types` and `exclude_functions` refine the already curated model for one backend.
- Adapter declarations replace an awkward Rust-native method shape with an explicitly described portable operation.

Source skips are valuable when an item is intrinsically not part of the polyglot contract. Configuration exclusions are valuable
when the same source checkout needs different projections, especially per target. Neither should be used to conceal an accidental
unknown type: critical validation, including lossy-surface validation, is intended to stop generation.

## Insight 3: “Alef backend” means a real language implementation

All backends implement a common trait, but the trait standardizes orchestration rather than forcing a universal ABI.
`alef/src/core/backend.rs` defines the backend name, language, capability flags, binding generation, and optional stub, scaffold,
public-surface, service, and build behavior. Capability flags include async, classes, enums, option/result, callbacks, streaming,
and services.

The supplied checkout contains these backend families:

| Target | Generated bridge or package strategy |
| --- | --- |
| Python | PyO3 extension plus optional `.pyi` |
| Node | NAPI-RS addon plus TypeScript declarations |
| WASM | `wasm-bindgen` package |
| Ruby | Magnus extension plus optional RBS |
| PHP | Native extension plus optional PHP stubs |
| Elixir | Rustler NIF |
| R | extendr package |
| Go | cgo over a generated C FFI layer |
| Java | JVM package with a native library |
| Kotlin/JVM | JVM-oriented Kotlin package |
| Kotlin/Android | Android JNI package and ABI layout |
| JNI | Lower-level JNI shim |
| C# | P/Invoke over native FFI |
| Dart | flutter_rust_bridge package |
| Swift | swift-bridge package |
| Gleam | Rustler-backed package |
| Zig | C-ABI wrapper and Zig package |
| FFI | C header and native C ABI |

The implementations live under `alef/src/backends/`. They make different ownership, error, async-runtime, naming, and package-layout
decisions. Even streaming is not one shared template: the common streaming adapter dispatcher handles a broad set of targets in
`alef/src/adapters/streaming.rs`, while the Swift backend has its own stream shims, extern declarations, and Swift client generation
under `alef/src/backends/swift/`.

This is why the intermediate representation matters. It centralizes the semantic contract—types, methods, documentation, serde
behavior, errors, and adapter intent—while allowing each backend to generate an idiomatic host API.

## Insight 4: generated output is a managed product surface

Binding source is only one class of Alef output. A backend returns `GeneratedFile` values with a path, content, and a flag
describing whether the file carries a generated header. The surrounding pipeline uses that distinction to separate regenerated files
from create-once project seeds.

Generated files are overwritten only when Alef can prove ownership. Existing unmarked files are not casually replaced; they require
an explicit adoption flow. Create-once files, such as editable package templates, are created when missing and then preserved. This
lets a generated package contain both an owned machine surface and intentional hand-edited seams.

Depending on the backend and configuration, Alef emits:

- wrapper crates, native loaders, host-language source, manifests, and build configuration;
- `.pyi` for Python, `.d.ts` for Node, RBS for Ruby, and PHP stubs—the current explicit stub implementations, not a promise that
  every target has stubs;
- per-language README files from templates and snippets or a fallback generator;
- language-specific API reference pages plus shared type, error, and configuration documentation;
- runnable end-to-end projects with local package references, JSON fixtures, generated calls, and semantic assertions.

The relevant orchestration is in `alef/src/cli/pipeline/`, with documentation in `alef/src/docs/`, E2E generation in
`alef/src/e2e/`, and target-specific material in `alef/src/backends/*`.

The E2E layer is particularly important architecturally: it operates from the same type model as code generation. It can therefore
validate host-language construction, enums, optionals, errors, async calls, and stream iteration instead of merely checking that a
native library linked.

## Insight 5: freshness is reconstructive, not just a timestamp check

Current Alef uses two different hashes:

- an inputs hash over a domain/version prefix, the code-generation format version, explicitly configured source contents, and
  canonicalized `alef.toml`;
- a per-file content hash over the generated content with its own hash line removed.

Canonical TOML normalization removes irrelevant formatting differences and the workspace Alef-version key. The implementation is
`alef/src/core/hash.rs`. A successful generation records the inputs baseline in `.alef-generation.toml`; ownership for files that
cannot carry comments, such as JSON, archives, or lockfiles, is recorded by path in `.alef-ownership.toml`.

`alef verify` is intentionally read-only. It checks more than the markers already on disk:

- current inputs against the committed generation baseline;
- embedded content hashes;
- missing generated files, including gitignored ones;
- files that generation would now refuse to overwrite;
- newly generated files versus the managed set;
- orphaned generated files that are no longer produced;
- generator-version, snippet-coverage, and native-ABI stamp consistency;
- target-specific consistency checks such as Dart bridge canonicalization.

To find missing, frozen, and orphaned output, verification regenerates the managed surface in memory and compares it with the
checkout. It reports orphans; it does not delete them. Drift in a create-once file is informational because Alef no longer owns that
content after creation. The implementation is `alef/src/bin_cli/core_commands/verify.rs`.

There are two limits worth stating plainly. First, without an existing generation baseline, Alef cannot prove that inputs are stale.
Second, markerless ownership proves that a path is managed, not that an arbitrary binary file's content matches an embedded hash.
Also, current `verify` is not a compiler: its legacy `--compile` and `--lint` compatibility flags are rejected as unimplemented.
Build, lint, and E2E remain separate gates.

liter-llm's 0.68.0-generated files use the earlier embedded recipe, so the 0.71.0 split between an inputs record and per-file
content hash should not be read backward into those artifacts.

## Insight 6: packaging is target-specific staging, not universal publishing

Alef's publish subsystem separates preparation, build, and packaging. It can vendor the core crate fully, vendor only the core,
rewrite to registry dependencies, or leave dependencies untouched. FFI-dependent targets can stage a native artifact for a host
package. The organizing code is `alef/src/publish/mod.rs`.

The current checkout has concrete build recipes for mature paths such as Python/maturin, Node/NAPI, WASM/wasm-pack, Ruby, PHP,
Elixir, R, C FFI, and the FFI-dependent Go, Java, and C# packages. Other targets rely on custom hooks or external CI for native
production builds. Package assembly covers more targets than the default build phase, but Kotlin/Android and raw JNI packaging
remain incomplete in the supplied implementation.

Swift illustrates the boundary. `alef/src/publish/package/swift.rs` can stage the source package and manifest, but it leaves a
placeholder XCFramework and checksum instructions. Producing the real Apple artifact requires macOS, `xcodebuild`, and release
automation outside that generic packager. Likewise, registry authentication and final publication belong in CI, not in Alef's local
preparation step.

Alef therefore supplies reproducible package structure and local staging; it does not claim that every target can be released from
every development host with one identical command.

## Insight 7: liter-llm makes its extraction inputs auditable

liter-llm's `[[crates]]` entry explicitly enumerates 24 Rust files in `liter-llm/alef.toml`. They form a useful architectural map:

- `src/error.rs` defines the portable failure contract.
- `src/bindings.rs` is the deliberately small construction façade.
- `src/types/{common,chat,embedding,image,audio,moderation,rerank,search,ocr,models,files,batch,responses}.rs` define the owned
  request/response graph.
- `src/image.rs` contributes data-URL helpers and their bindable result type.
- `src/client/mod.rs` contributes the concrete client and public operation signatures.
- `src/provider/custom.rs` and `src/provider/mod.rs` contribute portable provider configuration, catalog, and custom-provider
  registration types.
- `src/lib.rs` contributes the crate's canonical public re-exports.
- `src/cost/refresh.rs` contributes catalog-refresh configuration and results.
- `src/tower/{budget,cache,rate_limit}.rs` contribute the small subset of middleware policy data intended for foreign callers.

This list is broader than a minimal set of discovery roots. `src/lib.rs` could lead module traversal to part of the same graph, but
listing the contract files makes review intent explicit and, in current Alef's model, puts their contents in the source hash.
Implementation files for HTTP streaming, provider-specific transforms, and much of the Tower stack are not explicit roots. They
still compile into the Rust library and may be encountered through module traversal, but visibility and exclusions keep them out of
the final portable model.

The crate is extracted with `native-http` and `full` features. It also declares `AssistantContent` as an untagged union text type,
giving Alef an explicit hint for a serde representation whose intended host-language treatment cannot be inferred from a nominal
Rust type alone.

### The bindable data conventions are conservative

The selected DTOs are overwhelmingly owned, public, non-generic structs and enums using `String`, `Vec`, `Option`, maps, numeric
primitives, byte vectors, and `serde_json::Value`. They generally derive `Clone`, `Debug`, `Default`, `Serialize`, and
`Deserialize`, with serde names and defaults providing the wire contract. Examples are in
`liter-llm/crates/liter-llm/src/types/chat.rs` and the adjacent `types/` modules.

Where a Rust type would create an awkward boundary, liter-llm normalizes it. `WaitForBatchConfig`, for example, expresses a timeout
in scalar seconds rather than exposing a host language to Rust duration construction. `DecodedDataUrl` is a named result with
explicit fields rather than an anonymous tuple. These small choices preserve useful names and deterministic conversions in every
backend.

`DefaultClient` takes the opposite approach: it is intentionally opaque. Its private fields include configuration, an HTTP client,
cached headers, and an `Arc<dyn Provider>`. Foreign callers get a stable handle, not a generated representation of those internals.
See `liter-llm/crates/liter-llm/src/client/mod.rs`.

The error enum uses `thiserror` and gives Alef a structured error shape, but source skips remove payloads that cannot be represented
portably. The same source annotation mechanism hides native traits, erased aliases, and raw operations even before the
configuration-level exclusions are applied.

## Insight 8: `bindings.rs` is a construction seam, not a second API

`liter-llm/crates/liter-llm/src/bindings.rs` is only about a hundred lines. Its main functions are:

- `create_client`, which accepts an API key plus optional base URL, timeout seconds, retry count, and model hint, then drives the
  native builder;
- `create_client_from_json`, which deserializes `FileConfig`, maps a malformed document into the crate's bad-request error, and
  constructs the client.

The façade converts scalar seconds into `Duration`, hides builder type states, avoids exposing `Arc<dyn Provider>`, and returns the
same concrete `DefaultClient` used by the Rust API. It does not duplicate chat, embeddings, files, batches, or stream processing.
Those calls remain real methods on the client and are wired by adapter metadata.

This is an effective boundary pattern: add handwritten Rust only for construction or conversion that is intrinsically hostile to
foreign type systems, then let the generator project the ordinary DTO and client surface. The custom Swift constructor body in
`liter-llm/alef.toml` is a target-specific variation of the same idea; it builds `ClientConfig` and calls `DefaultClient::new`
without widening the general binding contract.

## Insight 9: adapters monomorphize the async contract

liter-llm's native client traits use erased return types such as `BoxFuture` and `BoxStream`. Those are appropriate for a
provider-polymorphic Rust API and poor public types for most host languages. The corresponding trait declarations are
source-skipped, while the concrete client is exposed as an opaque owner.

The adapter entries near the end of `liter-llm/alef.toml` name each portable operation. Async entries specify the owner, core
method, parameters, result, and `LiterLlmError` for chat, embeddings, model listing, image generation, transcription, moderation,
reranking, search, speech, OCR, files, batches, and Responses operations.

The stream entry is equally explicit:

- owner: `DefaultClient`;
- core method: `chat_stream`;
- request: `ChatCompletionRequest`;
- item: `ChatCompletionChunk`;
- error: `LiterLlmError`.

This metadata is not a second implementation of streaming. It is a portable monomorphization recipe: “call this method on this
opaque owner, and present this concrete request/item/error contract.” Each backend chooses the runtime and iteration mechanism
appropriate to its language.

## Insight 10: a Swift stream crosses three ownership domains

The checked-in Swift package makes the full crossing visible.

### 1. Swift enters through an idiomatic API

`DefaultClient.chatStream` in `liter-llm/packages/swift/Sources/LiterLlm/LiterLlm.swift` accepts a `ChatCompletionRequest` and
returns `AsyncThrowingStream<ChatCompletionChunk, Error>`. The public API does not expose a Rust future, Tokio handle, `BoxStream`,
or C callback.

The request and client are bridged objects. Alef also generates unchecked `Sendable` conformances because the Swift task moves them
across concurrency domains while Rust enforces the actual thread safety.

### 2. The generated Rust shim acquires the native stream

The Swift-facing Rust crate owns a process-wide multi-threaded Tokio runtime in `liter-llm/packages/swift/rust/src/lib.rs`. Its
generated `default_client_chat_stream_start` function:

1. borrows the opaque client and request wrappers;
2. clones the native request;
3. blocks on the async `DefaultClient::chat_stream` initiation;
4. erases each `LiterLlmError` into a boxed error;
5. stores the resulting `'static` stream behind a mutex in an opaque handle.

Initiation failure is flattened to `String` at this bridge boundary.

### 3. The native core performs the actual request and parsing

`DefaultClient::chat_stream` in `liter-llm/crates/liter-llm/src/client/mod.rs` validates and prepares the request with streaming
enabled, resolves the provider and URL, applies authentication or signing, and selects the provider's stream format.

For SSE, `liter-llm/crates/liter-llm/src/http/streaming.rs` drives `reqwest::Response::bytes_stream` through a bounded parser
buffer. It handles UTF-8 split across chunks, `[DONE]`, provider-directed skips, parsed chunks, and parser or truncation errors. For
AWS event streams, the client selects the Bedrock event-stream path, whose framing and checksum validation remain a Rust-only
transport concern. Provider parsers and transforms turn those wire events into `ChatCompletionChunk` values before Alef sees them.

The ordinary `chat_stream` path uses `post_stream`, not the separate explicit `CancellationToken` variant. Dropping the owned
response stream releases its transport resources, but the public generated call does not propagate a dedicated cancellation token
into the core.

### 4. Swift pulls one item at a time

The opaque stream handle's generated `next()` method locks the stream, uses the same Tokio runtime to block on `StreamExt::next`,
and returns one of:

- a chunk serialized as JSON;
- an error converted to a string, after clearing the stream;
- an empty string sentinel at end of stream, also after clearing it.

JSON here is a deliberate local bridge protocol. It avoids depending on swift-bridge support for an optional opaque item while
preserving the public Swift `Codable` value type.

Back in Swift, a detached task repeatedly calls `next()`, decodes each JSON string with `JSONDecoder`, and yields it to the
`AsyncThrowingStream`. Termination cancels the Swift task. Because `next()` itself is a synchronous bridge call that may be blocked
inside Rust, Swift task cancellation is not guaranteed to interrupt an already in-flight poll immediately; eventual handle
destruction drops the native stream. That limitation follows from the checked-in code rather than from the public `AsyncSequence`
shape.

The remaining bridge declarations and C headers are generated under `liter-llm/packages/swift/Sources/RustBridge/`. Swift E2E tests
consume the result with `for try await`, exercising the same public iteration model a user sees.

### 5. Development and release packages close the loop differently

`liter-llm/packages/swift/Package.swift` is the development package and links local native libraries. The repository-root
`liter-llm/Package.swift` is the release-facing binary package. `liter-llm/.github/workflows/publish.yaml` builds the Apple artifact
bundle on suitable runners, calculates its checksum, uploads it, and patches the release manifest. This is the concrete production
completion of the generic Alef Swift scaffolding and placeholder packager.

## Insight 11: Node uses a bounded async-generator pump

The Node target exposes the same native call with a different concurrency shape. In `liter-llm/crates/liter-llm-node/src/lib.rs`:

1. `JsDefaultClient` holds an `Arc<DefaultClient>`.
2. `chatStream` converts the generated JavaScript request to the Rust DTO.
3. It creates a Tokio MPSC channel with capacity 32 and spawns a task.
4. The task awaits native stream creation, then pulls chunks and sends converted results through the channel.
5. `ChatStreamIterator`, whose receiver is mutex-protected, implements NAPI's async-generator interface by awaiting the next channel
   value.

The declaration in `liter-llm/crates/liter-llm-node/index.d.ts` is `Promise<AsyncGenerator<ChatCompletionChunk, void, undefined>>`,
so JavaScript consumes it with `for await`. Initiation and item errors become NAPI failures; an item error is sent once and
terminates the pump.

The capacity-32 channel introduces bounded decoupling between Rust network parsing and JavaScript iteration. If the host is slow,
sends eventually await capacity rather than allowing an unbounded queue. If the receiver closes, the send fails, the pump breaks,
and dropping the native stream releases the request. The exact timing of early JavaScript iterator destruction depends on the NAPI
object's lifetime and cannot be established more strongly from this generated Rust alone.

Swift and Node thus share the same request DTO, opaque client, native core method, chunk type, and adapter declaration. They
intentionally do not share the same runtime bridge: Swift is a pull handle driven by a detached task; Node is a bounded push pump
surfaced as an async generator.

## Insight 12: the Rust-only surface is large by design

liter-llm's global exclusions in `liter-llm/alef.toml` are an architectural inventory of what should remain native. Major categories
include:

- client traits, raw client methods, `BoxFuture`, `BoxStream`, builders, and type-state construction machinery;
- prepared requests, raw transports, HTTP internals, provider trait objects, credential resolution, signing, and header assembly;
- Tower layers, services, retry and circuit policies, hooks, routing, discovery, health state, ledgers, metrics internals, and
  erased sinks;
- realtime translation and streaming-pipeline internals;
- tenant credential and key-resolution implementations;
- vector-store implementations and model-pricing internals;
- file-based top-level configuration and managed-client construction;
- streaming Responses event types whose handwritten serde and newtype variants do not yet form an auto-bindable surface.

The exclusion list also removes raw methods such as `chat_raw` and `chat_stream_raw`, internal client preparation and authentication
methods, builder-returning functions, and helpers whose sanitized types would fail Alef's hard surface validation.
`DefaultClient.create_response_stream` remains Rust-only in this release even though ordinary chat streaming is exported.

This is not missing generator coverage disguised as a full export. It is a deliberate public-contract decision: foreign packages get
the common product operations and stable DTOs; Rust retains extension points whose value depends on Rust traits, generics,
lifetimes, or ecosystem-specific composition.

## Insight 13: target exclusions document real portability edges

After the global projection, liter-llm applies a small set of target-specific changes in `liter-llm/alef.toml`:

| Target | Additional exclusion or packaging restriction |
| --- | --- |
| Node | Omits the two musl platform packages from its release matrix |
| PHP | Excludes `ensure_crypto_provider` because the PHP macro cannot resolve its `Self` call |
| WASM | Excludes nine policy/state types and the two tokenizer functions |
| JNI | Excludes the two tokenizer functions |
| Swift | Excludes `all_providers`; also supplies a custom client constructor |
| Kotlin/Android | Excludes five policy types and the two tokenizer functions; builds arm64-v8a and x86_64 ABIs |

The nine WASM types are `BudgetConfig`, `CacheConfig`, `CacheBackend`, `RateLimitConfig`, `Enforcement`, `CircuitState`,
`HealthStatus`, `IntentPrototype`, and `SingleflightResult`. Kotlin/Android excludes the first five of those. The configuration
explains the PHP exclusion, but it does not state a definitive reason for each WASM, JNI, Swift, or Android exclusion; it would be
speculation to invent one.

There is also an important difference between configured intent and current release readiness. The workspace declares 15 Alef
targets: Python, Node, Ruby, PHP, C FFI, Go, Java, C#, Elixir, WASM, Zig, Dart, Swift, Kotlin/Android, and JNI. These yield 13
host-language package families plus the C and JNI infrastructure layers. The checked-in release workflow currently forces
Kotlin/Android publishing off because generated `SingleflightResult.kt` references a native free function that is absent from both
bridge sides. The comment and gate are explicit in `liter-llm/.github/workflows/publish.yaml`; the package is configured, but this
checkout does not claim it is presently releasable.

## Architectural judgment

Alef's strongest idea is not any individual backend. It is the separation of one audited semantic model from many target-specific
implementations, combined with ownership and freshness rules for the entire generated package. That makes ordinary Rust source
usable as the contract without pretending every ordinary Rust abstraction is portable.

liter-llm uses that idea well because it does not force its internal design to be foreign-language-shaped. Provider polymorphism,
transport parsing, runtime composition, and raw extension points stay idiomatic Rust. The exported model is intentionally more
conservative: owned serde DTOs, one opaque client, small scalar constructors, and declared async/stream adapters.

The costs are real. Source extraction is necessarily less authoritative than rustc, explicit roots must be maintained for complete
freshness provenance, target exclusions can drift, and some native release artifacts still require bespoke CI. Streaming also
exposes backend-specific cancellation and buffering semantics even when the public operation is nominally the same.

Those are manageable costs because the boundary is visible. In this pairing, `alef.toml`, the selected Rust files, generated
packages, E2E projects, and release workflow together form the polyglot contract. The architecture works not by erasing language
differences, but by keeping them localized in Alef's backends while liter-llm presents a small, explicit, portable core.
