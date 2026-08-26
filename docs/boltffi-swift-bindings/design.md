# BoltFFI-generated Swift bindings design

Status: design finding, 2026-08-25. Scope: the `core` and `extended` rows in
[`api-inventory.md`](api-inventory.md), targeting the Send/Tokio family and
Swift. The current crate names are used throughout. When the crates are renamed,
`pi-ai`, `pi-agent-core`, `pi-agent-runtime-tokio`, and `pi-agent-session` become
the corresponding `agentprism-*` packages; this is a packaging/name update, not
a change to the mappings below.

> **Compatibility verdict: R1, R2, and R3 cannot all be satisfied by the
> documented BoltFFI surface.** The ordinary Rust API returns borrowed
> `SendBoxStream` values, `AssistantStream`, and Tokio receivers, whereas the
> documented stream attribute requires the annotated method itself to return
> `Arc<EventSubscription<T>>` (`crates/pi-agent-core/src/run.rs:283`,
> `crates/pi-ai/src/streaming.rs:1900`,
> `crates/pi-agent-runtime-tokio/src/lib.rs:133`). BoltFFI documents exactly that
> return-type requirement and generates Swift `AsyncStream<T>` only from that
> shape. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
> The host callback contracts use explicit `SendBoxFuture` return values, while
> the documented trait mapping requires an exported trait method to be written
> as `async fn` with `#[async_trait]` to become an async Swift protocol
> requirement (`crates/pi-agent-core/src/tools.rs:201`,
> `crates/pi-agent-runtime-tokio/src/lib.rs:45`,
> `crates/pi-agent-session/src/storage.rs:18`). [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
> Generic free functions, generic structs, generic traits, associated types,
> arbitrary trait objects, and non-static lifetimes cover further core or
> extended items, and the documentation explicitly lists those shapes as
> unsupported. Generic inherent methods and constructors, generic enums, and
> generic type aliases are treated separately as documentation gaps below.
> [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]
> [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
> [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]

This is therefore a mapping and feasibility design, not a claim that the whole
surface currently compiles through BoltFFI. A row marked **direct** identifies
the documented construct to apply once all transitive field and signature types
are representable. A row marked **blocked** identifies the exact reason an
attribute-only implementation cannot expose the native item. No envelope API
is substituted for a native item.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]

## 1. BoltFFI capability summary

The snapshot manifest records nineteen canonical `/docs/` pages fetched on
2026-08-25 and no unreachable documentation links
(`docs/boltffi-swift-bindings/docs-snapshot/MANIFEST.md:1`). Each page was read
in full for this design.

- **Overview.** BoltFFI describes generated native bindings, lists Swift among
  the supported targets, and identifies records, functions, classes, constants,
  async functions, callbacks/traits, async streams, and errors as exportable
  categories. [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export]

- **Installation.** Installation adds `boltffi` as both a normal and build
  dependency, configures a `staticlib` crate type (with `cdylib` optional), puts
  `boltffi::build::generate()` in `build.rs`, and verifies the project with
  `boltffi check`. [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]

- **Quick Start.** The quick start repeats the Cargo and `build.rs` setup, uses
  `#[data]` and `#[export]` after importing them from `boltffi`, and shows initialization, Apple
  build, Swift generation, and XCFramework packaging commands.
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#2-configure-cargotoml]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#3-create-buildrs]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#4-write-your-rust-code]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#5-build-and-generate-bindings]

- **Getting Started.** The page introduces `#[data]` for a value record,
  `#[export]` for a free function after `use boltffi::*`, and the choice among build, generate,
  and package operations before importing the generated Swift module.
  [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
  [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#build-package-or-generate]

- **Tutorial.** The tutorial combines a data record, an exported Rust-backed
  class, fallible methods, and async methods, then shows those results as Swift
  value types, classes, `throws`, and `async` calls.
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#write-the-rust-code]
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#adding-error-handling]
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#adding-async]

- **Functions.** `#[export]` exports free functions and supports the
  documented primitive, string, record, enum, slice, optional, class,
  callback-trait, `Option`, `Result`, `Vec`, async, and non-stored closure forms;
  generic free functions, references returned by free functions, and stored or
  outliving closures are listed as limitations. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#slices]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#optional]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#callback-traits]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#option]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#result]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#vec]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#async-functions]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#closures]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]

- **Records.** `#[data]` maps structs to Swift value structs and maps
  simple or payload enums to Swift enums; `#[data(impl)]` exports record
  constructors and methods, and an `&mut self` method becomes mutating Swift.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#mutating-methods]

- **Classes.** `#[export]` on an inherent `impl` maps a Rust-owned object
  to a reference-semantics Swift class; constructors return `Self`, methods may
  be synchronous, asynchronous, static, fallible, or class-valued, and `#[skip]`
  omits a method. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
  Exported classes require `Send + Sync` by default; `#[export(single_threaded)]`
  disables both that check and the mutable-receiver check, permits `&mut self`,
  and makes target-side serialization the consumer's responsibility.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]

- **Constants.** `#[export]` maps supported global constants to module
  values and constants inside exported data/class impls to static members; the
  documented values are primitives, strings, and byte slices.
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#global-constants]
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]

- **Types.** The type tables map the listed primitives, strings, `Option`, `Result`, and
  `Vec`, and provide built-in mappings from `Duration`, `SystemTime`,
  `uuid::Uuid`, `url::Url`, and `Vec<u8>` to Swift `TimeInterval`, `Date`,
  `UUID`, `URL`, and `Data` respectively.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types]
  The overview lists `HashMap` among exportable categories, but the type-page
  quick reference and collections sections do not give a `HashMap`-to-Swift
  mapping or specify nested-map support. **UNRESOLVED: not answered by the
  documentation**; pages checked: `overview.md#what-you-can-export`,
  `types.md#quick-reference`, `types.md#collections`.
  [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections]
  Generic structs, arbitrary `dyn Trait`, raw pointers, non-static lifetimes,
  and `HashSet` are listed as unsupported.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]

- **Callbacks & Traits.** `#[export]` on a trait generates a Swift
  protocol implemented by the host; `Box<dyn Trait>` transfers single ownership,
  `Arc<dyn Trait>` permits shared ownership, async protocol requirements use
  `#[async_trait]` and actual `async fn` methods, stored callbacks must be owned,
  and multithreaded callbacks require `Send + Sync`.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#storing-traits]
  Generic traits and associated types are unsupported, and default implementations
  are ignored. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]

- **Async.** An exported Rust `async fn` becomes a Swift `async` function and an
  async function returning `Result` becomes `async throws`; target-task
  cancellation propagates cooperatively to the Rust future.
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#standalone-functions]
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
  BoltFFI does not provide an executor; target callbacks drive future polling,
  and Tokio-dependent work must already have an active Tokio runtime.
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]

- **Async Internals.** The generated async ABI uses entry, poll, complete,
  cancel, and free operations with continuation callbacks, and cancellation
  marks the future, wakes it, and frees the task before the target wrapper
  reports cancellation. [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#generated-ffi-functions]
  [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]

- **Streaming.** `#[ffi_stream(item = T, mode = "async")]` may annotate
  only a method returning `Arc<EventSubscription<T>>`, and that method becomes
  Swift `AsyncStream<T>`; callback mode produces a cancellable subscription and
  batch mode produces pull-based batches.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stream-modes]
  `StreamProducer<T>` broadcasts to subscribers through a default 256-item ring
  buffer, never blocks the producer, and drops new events when a subscriber's
  buffer is full. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#creating-streams]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
  Producer unsubscribe finishes iteration, while cancelling the Swift task or
  breaking its loop cancels the subscription.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stopping-streams]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#consumer-side-cancellation]

- **Errors.** An exported `Result<T, E>` becomes a throwing target call when
  `E` is a string, `#[error]` struct, or `#[error]` enum;
  simple and payload error enums map to Swift error enums, and the same mapping
  applies to async results. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]

- **Custom Types.** For an external or otherwise non-native boundary type,
  `custom_type!` defines conversions to a supported representation, while
  `#[custom_ffi]` plus `CustomFfiConvertible` provides manual owned-type
  conversion; representation types may be primitives, `String`, `Vec`, or a
  BoltFFI data type and may be nested in containers.
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-custom_type-macro]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]
  A failed custom conversion panics rather than returning a recoverable boundary
  error. [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors]

- **Configuration.** A root `boltffi.toml` selects package identity, source
  crate, Apple output and deployment settings, Swift module name, SwiftPM
  layout, slices, symbols, and the limited documented type-mapping overrides.
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#apple-configuration]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#type-mappings]

- **Packaging.** `boltffi generate swift` generates source only, whereas
  `boltffi pack apple` builds Rust, generates Swift, and produces an XCFramework
  and Swift package; the Apple package supports bundled, split, and FFI-only
  layouts. [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#overview]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#swiftpm-layouts]

- **Experimental Features.** Experimental mode is enabled by CLI flag or
  configuration and the listed experimental stream work concerns Kotlin
  Multiplatform and TypeScript, not Swift.
  [https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#enabling]
  [https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#feature-details]

## 2. Integration shape and R2 verdict

### Required project attachment

For the single Rust library shown by the documentation, setup is a normal
`boltffi` dependency, a build dependency on `boltffi`, a library crate type that
includes `staticlib`, and a `build.rs` calling `boltffi::build::generate()`.
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
The current `pi-ai`, `pi-agent-core`, `pi-agent-runtime-tokio`, and
`pi-agent-session` manifests contain none of that setup
(`crates/pi-ai/Cargo.toml:1`, `crates/pi-agent-core/Cargo.toml:1`,
`crates/pi-agent-runtime-tokio/Cargo.toml:1`,
`crates/pi-agent-session/Cargo.toml:1`). Those manifest and build-script edits
would be required wherever the documented single-library setup is applied,
although they do not alter library contracts. Whether one binding package can
discover annotated dependency crates is unresolved below.

The proposed gate is a `boltffi` Cargo feature in each participating crate, with
ordinary Rust conditional attributes on existing items:

```rust
#[cfg(feature = "boltffi")]
use boltffi::*;

#[cfg_attr(feature = "boltffi", data)]
pub struct PromptImage {
    pub data: String,
    pub mime_type: String,
}
```

**UNRESOLVED: not answered by the documentation** — whether BoltFFI's discovery
and generated build support `cfg_attr`, and whether the dependency and build
script themselves may safely be optional. Pages checked:
`installation.md#add-to-your-project`, `installation.md#create-buildrs`,
`getting-started.md#write-your-code`, and
`configuration.md#package-identity`.
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
[https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]

The exact intended attribute placement is:

| Existing item kind | Attribute on the existing item | Scope in this repository |
|---|---|---|
| Concrete value struct or enum whose entire field graph is supported | Under the documented `use boltffi::*` import, put the proposed `#[cfg_attr(feature = "boltffi", data)]` on the item | IDs, messages, replay, events, options, outcomes, catalog/auth/session values after their transitive blockers are resolved. `#[data]` is the documented record/enum attribute. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| Inherent methods on a data record | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", data(impl))]` on an existing `impl` only when every public method in that impl has a documented signature | Concrete constructors, `&self` getters returning owned values, documented `&mut self` record mutators, and static methods. `#[data(impl)]` is documented, but record-method omission is not. **UNRESOLVED: not answered by the documentation** — `#[skip]` is shown only inside class `#[export]` impls, not `#[data(impl)]`; pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. Consequently a mixed record impl cannot be selectively exported attribute-only on the present evidence. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| Method defined by a trait implementation | No attribute placement is authorized by the snapshot | `Models::default` is defined in `impl Default for Models` and `ApiRequestOptions::from` is defined in `impl From<&SimpleGenerationOptions> for ApiRequestOptions`; neither is an inherent implementation (`crates/pi-ai/src/models.rs:99`, `crates/pi-ai/src/options.rs:468`). The record and class pages demonstrate annotated inherent implementations, not exporting methods from `impl Trait for Type`. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `records.md#static-methods`, `classes.md#defining-a-class`, `classes.md#static-methods`. An inherent forwarding method would change the crate API and therefore violate R2. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods] |
| Rust-owned reference object with only documented `&self`/static methods | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", export)]` on the existing inherent `impl`; class-only omissions may use proposed `#[cfg_attr(feature = "boltffi", skip)]` | `AgentControl`, `CancellationToken`, `Models`, `TokioAgentHandle`, and eligible in-memory session impls, subject to every public signature and the default `Send + Sync` compile-time check. The struct and its private fields stay private; only impl methods are exposed. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| Rust-owned reference object with an existing `&mut self` method | Put proposed `#[cfg_attr(feature = "boltffi", export(single_threaded))]` on the existing class impl, with class `#[skip]` only where omitting a method is acceptable | Required for the `impl crate::Agent` containing `set_tool_execution_mode`, `reset_transcript`, and `reset_all` (`crates/pi-agent-core/src/run.rs:138`), and for `impl TokioAgentRun` containing `next_event` (`crates/pi-agent-runtime-tokio/src/lib.rs:131`). The same attribute would be required if `CustomRecordKinds`, `ToolRegistry`, or `CommittedEventReplay` are exposed as classes with their existing mutators. This mode disables both the `Send + Sync` and `&mut self` checks and makes target-side serialization the consumer's responsibility. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] |
| Free function or supported constant | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", export)]` on that existing item | Migration, replay, OAuth, and constants listed in the inventory when their signatures are concrete and supported. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions] [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#global-constants] |
| Throwable error struct or enum | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", error)]` on that existing error item | `AgentError`, `ControlError`, `ToolError`, `ToolUpdateError`, `RequestStartError`, provider/auth/catalog/options/session errors, and `CancellationError`. Error structs and enums are the documented `Result` error mapping. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| Host-implemented callback trait with documented signatures | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", export)]` on the existing trait | Synchronous concrete traits can be candidates; async traits qualify only if their methods are actual `async fn` under the documented form. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| Stream-producing class method | Under the documented wildcard import, put the proposed `#[cfg_attr(feature = "boltffi", ffi_stream(item = AgentEvent, mode = "async"))]` or the `AssistantEvent` equivalent on the method | **No existing method qualifies:** the documented attribute requires `Arc<EventSubscription<T>>`, which no inventoried native stream method returns. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] |

For the two rejected mutable-class cases, the exact partial attachment would be
`#[cfg_attr(feature = "boltffi", export(single_threaded))]` on the existing
`impl crate::Agent` at `crates/pi-agent-core/src/run.rs:138` and on the existing
`impl TokioAgentRun` at `crates/pi-agent-runtime-tokio/src/lib.rs:131`. The
former is the only documented class mode that permits its
`set_tool_execution_mode`, `reset_transcript`, and `reset_all` mutable
receivers; the latter is the only documented class mode that permits
`next_event(&mut self)`. That mode disables both checks and places serialization
on the Swift consumer.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]

Compiling either impl as a partial experiment would additionally require the
documented class `#[skip]` on methods whose signatures remain blocked—for
example, the bare Agent stream methods and `TokioAgentRun::{events,outcome}`.
That is not the R1 design: omitting inventoried methods fails the requirement,
and `#[skip]` has no documented record-impl counterpart. Therefore no complete
attribute set is proposed for either impl until the stream and consuming-method
gaps are resolved. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]

Likewise, no `#[data(impl)]` is proposed for the mixed record impls named in
section 3, and no `#[export]` is proposed for the consuming
`ModelsBuilder`/`ProviderRegistrationBuilder` impls. The record and class method
pages do not answer how to expose those consuming transitions, and the record
page provides no selective omission attribute. **UNRESOLVED: not answered by
the documentation**; pages checked: `records.md#methods-and-constructors`,
`classes.md#methods`, `classes.md#skipping-methods`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]

The code-generation flow is: initialize root configuration, run
`boltffi check`, generate source with `boltffi generate swift` for iteration,
then produce the distributable Apple artifact with `boltffi pack apple`.
[https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#5-build-and-generate-bindings]
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]
[https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
The root configuration can set the Swift module name and package layout.
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swift-module-name]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swiftpm-layouts]

### R2 verdict

**R2 fails for the inventoried surface.** Even the smallest documented setup
requires Cargo/build-script/crate-type changes in addition to item attributes.
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
More importantly, native stream methods need new adapter methods returning
`Arc<EventSubscription<T>>`, unsupported external/recursive types need
conversion implementations, documented generic free functions/structs/traits
need concrete counterparts, boxed-future traits need signature changes or
adapter traits,
whole owner-defined values with unsupported fields need manual conversion,
and consuming or mixed record impls have no documented direct resolution.
Generic inherent methods/constructors, generic enums, and generic aliases also
have no documented resolution.
Naked `i128`/`u128` arguments and returns, methods defined only by trait
implementations, borrowed data-record/enum inputs, and owned Rust-class inputs
have no documented direct mapping. Mutable Agent/run methods are attribute-only
only through `#[export(single_threaded)]`, which transfers serialization
responsibility to Swift. The documented nearest mechanisms are an
`EventSubscription<T>` stream method, whole-type `custom_type!` or
`CustomFfiConvertible` conversion, concrete nongeneric exports, and exported
`async fn` callback traits respectively; the snapshot gives no corresponding
mechanism for the four naked-signature/trait-impl/input-direction gaps.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#creating-streams]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods]
Those are contract additions or conversion code, so they are forbidden by R2.

## 3. Mapping table

### Reading the table

The selected boundary is the **Send/Tokio family**. Exported classes are
`Send + Sync` by default, while the single-threaded class mode exists for objects
that cannot meet that bound. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
Because the application-facing actor is explicitly Tokio/Send and the native
contracts carry `Arc`, `SendBoxFuture`, and `SendBoxStream`, choosing the Local
family would expose a different Rust contract. The Local rows remain the
inventory's stated exclusions.

Item paths are shortened after their first occurrence to keep the table
readable. In particular, the `TokioAgentHandle`, `TokioAgentRun`,
`AgentEventSink`, `EventSinkId`, and `TokioAgentError` rows retain the inventory's
`pi_agent_runtime_tokio` namespace even where the prefix is not repeated.

Coverage accounting: the inventory contains 382 rows whose Relevance cell
includes `core` or `extended`: 380 pure `core`/`extended` rows plus two mixed
rows. Every one of those 382 rows is represented in this section or by a
cross-cutting gap in section 5; grouped mapping rows do not remove an inventory
row from that accounting.

Each status has this exact meaning:

- **Direct data**: add `#[data]` after importing the macro; a supported struct or payload enum
  becomes a Swift value struct or enum. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
- **Direct class/method**: add `#[export]` to an inherent `impl` whose exposed
  instance methods take `&self`; an existing supported method becomes a Swift
  class method, and the class must pass the default `Send + Sync` check.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
- **Single-threaded class/method**: an existing class method taking `&mut self`
  requires `#[export(single_threaded)]`; that attribute disables both the
  default `Send + Sync` check and the ban on mutable receivers, leaving all
  target-side serialization to the consumer.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]
- **Direct async/throws**: an existing `async fn` maps to Swift `async`, and its
  `Result` maps to `throws`. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
- **Direct protocol**: add `#[export]` to a concrete host-implemented
  trait whose method signatures otherwise qualify. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
- **Direct error**: add `#[error]` to a struct or enum used as `Result`'s
  error, yielding a Swift `Error` and throwing calls. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
- **Direct constant/function**: add `#[export]` to a supported constant
  or free function. [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#global-constants]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions]
- **Blocked — documented generic/reference/type/trait/stream shape**: the
  indicated native shape exceeds a documented boundary. Generic free functions
  and references returned by free functions are unsupported; generic structs,
  arbitrary trait objects, and non-static lifetimes are unsupported; generic
  traits and associated types are unsupported; a generated stream requires
  `Arc<EventSubscription<T>>`.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
- **UNRESOLVED: not answered by the documentation — consuming method**: the record and class method pages document
  constructors/static methods, `&self`, and (under their respective rules)
  `&mut self`, but do not document an instance method that consumes an existing
  value with `self`. Pages checked: `records.md#methods-and-constructors` and
  `classes.md#methods`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
- **UNRESOLVED: not answered by the documentation — borrowed data input**:
  the function page demonstrates owned `#[data]` struct/enum arguments, `&str`,
  slices, and `&Class`, but it does not demonstrate `&Record` or `&Enum`.
  Pages checked: `functions.md#primitives-and-strings`,
  `functions.md#structs-and-enums`, `functions.md#slices`,
  `functions.md#classes`, and `records.md#instance-methods`.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#slices]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods]
- **UNRESOLVED: not answered by the documentation — owned class input**: the
  function and class pages demonstrate `&Logger`/`&User` class parameters and
  returning a class by value, but do not demonstrate passing a Rust-backed
  class as an owned argument. Pages checked: `functions.md#classes` and
  `classes.md#methods-that-take-or-return-classes`.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
- **UNRESOLVED: not answered by the documentation — trait-implementation
  method**: the record and class pages demonstrate annotated inherent impls,
  but do not describe exporting methods defined by `impl Trait for Type`.
  Pages checked: `records.md#methods-and-constructors`,
  `records.md#static-methods`, `classes.md#defining-a-class`, and
  `classes.md#static-methods`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods]

For record methods, “direct” applies to the whole existing `#[data(impl)]`
block only when all of its public methods qualify. The documentation does not
authorize `#[skip]` in `#[data(impl)]`, so mixed impls such as `AssistantEvent`
(`is_terminal` plus reference-returning `terminal_message`), `Usage` (`zero`
plus `u128`-returning methods), `Currency` (`usd` plus an unresolved generic
constructor and a non-static-reference method), `SessionEntry`,
`ProvisionedEntry`, and `OperationRecord` cannot
selectively expose only their supported members under R2. **UNRESOLVED: not
answered by the documentation**; pages checked: `records.md#methods-and-constructors`
and `classes.md#skipping-methods`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]

“Direct” is always conditional on the complete transitive field graph being
supported. A blocked leaf blocks every enclosing value even when the enclosing
struct or enum is itself the documented record shape.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs]

### `pi_agent_core` and Tokio actor

| Inventory item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `Agent` (`crates/pi-agent-core/src/restore.rs:128`) | Class candidate: the struct and its fields stay private and only methods on annotated impls are exposed, so its private trait-object fields are not a data-record representability question. The run impl contains multiple `&mut self` methods and therefore requires `#[export(single_threaded)]`, which disables both default safety checks and makes Swift-side serialization mandatory; constructor and method signatures remain independently blocked below. **UNRESOLVED: not answered by the documentation** — whether two existing inherent impl blocks for one class can both be annotated and merged; pages checked: `classes.md#defining-a-class`, `classes.md#methods`, `classes.md#single-threaded-mode`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] |
| `Agent::new`, `Agent::restore` (`crates/pi-agent-core/src/run.rs:140`, `crates/pi-agent-core/src/restore.rs:153`) | Blocked — both accept `Arc<dyn ModelRuntime>` and an owned `ToolRegistry`; restore also accepts callback trait objects. The callback docs cover host implementations crossing as `Box`/`Arc`, but do not document passing a Rust library implementation of a protocol back into another generated Rust class. Separately, the class-parameter examples use borrowed `&Class` and do not authorize the owned `ToolRegistry` input. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#ownership`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`, `types.md#whats-not-supported`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `Agent::{state,runtime,tools,snapshot}` (`crates/pi-agent-core/src/restore.rs:194`) | `snapshot` is intended as a direct owned class method after the snapshot graph maps. `state`, `runtime`, and `tools` return values containing references or arbitrary trait objects and are blocked by the global type restrictions on non-static lifetimes and trait objects. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `Agent::{state_mut,options,options_mut}` (`crates/pi-agent-core/src/run.rs:227`) | Blocked — their returned mutable and shared references carry non-static lifetimes, which the supported type boundary rejects. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AgentState`, `AgentState::new` (`crates/pi-agent-core/src/state.rs:23`) | Intended direct data, blocked transitively by `AgentRecord::Custom(Box<RawValue>)`, which has no documented built-in mapping. `AgentState::new` uses `impl Into<String>`, but the free-function limitation does not establish a rule for generic inherent constructors. **UNRESOLVED: not answered by the documentation** for this constructor; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `AgentRecord`, `AgentRecord::{message_id,custom_type_name}` (`crates/pi-agent-core/src/state.rs:62`, `crates/pi-agent-core/src/state.rs:158`) | Intended payload enum, blocked by `Box<RawValue>`; both inspection helpers return references with non-static lifetimes and are also blocked. Payload enums are otherwise direct data. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AgentSnapshot`, `AgentSnapshot::new` (`crates/pi-agent-core/src/state.rs:180`) | Intended direct data, blocked by the transcript graph and undocumented `Arc<[ToolCallId]>` field; `Vec` is documented but `Arc<[T]>` is not. **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#collections`, `records.md#structs`, `custom-types.md#containers`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers] |
| `AGENT_STATE_SCHEMA_VERSION`, `AGENT_SNAPSHOT_SCHEMA_VERSION`, `AGENT_INITIAL_SEQUENCE` (`crates/pi-agent-core/src/state.rs:10`) | Direct constants because their scalar types are among the documented constant values. [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] |
| `ModelRefResolver`, `CustomRecordKindRegistry` (`crates/pi-agent-core/src/restore.rs:21`) | Intended host protocols, but `ModelRefResolver::resolves(&ModelRef)` has a borrowed data-record input and `CustomRecordKindRegistry` has borrowed-string inputs. `&str` is documented, but `&Record` is not. **UNRESOLVED: not answered by the documentation** for the borrowed `ModelRef`; pages checked: `callbacks.md#traits`, `functions.md#primitives-and-strings`, `functions.md#structs-and-enums`, `functions.md#classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] |
| `CustomRecordKinds::{new,register}` (`crates/pi-agent-core/src/restore.rs:52`) | Class candidate; `register(&mut self, impl Into<String>)` would require `#[export(single_threaded)]` if made class-exportable. Whether the generic inherent method itself can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. A record-impl choice also cannot document-select only `new`; record `#[skip]` is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `migrate_agent_snapshot` (`crates/pi-agent-core/src/restore.rs:87`) | Intended direct free function after the blocked owned `AgentSnapshot` graph maps; its argument is owned data, the form demonstrated by the function page. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] |
| `AgentError` (`crates/pi-agent-core/src/error.rs:14`) | Intended direct error enum. Its `#[non_exhaustive]` behavior in generated Swift is undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `errors.md#enum-errors`, `records.md#enums`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `AgentInput`, `AgentInput::records` (`crates/pi-agent-core/src/run.rs:68`) | Intended direct data, blocked by `AgentRecord`. `AgentInput::records` uses `impl IntoIterator`, but the free-function limitation does not establish a rule for generic inherent methods. **UNRESOLVED: not answered by the documentation** for this method; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `PromptImage`, `PromptText` (`crates/pi-agent-core/src/run.rs:84`, `crates/pi-agent-core/src/run.rs:93`) | Direct data: fields are strings and `Vec<PromptImage>`, all documented record/container shapes. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs-with-strings-or-collections] |
| `Agent::{run,prompt_text,prompt_records,continue_run,retry_last_turn}` (`crates/pi-agent-core/src/run.rs:283`) | Blocked — native methods return borrowed `SendBoxStream<'a, AgentEvent>`, not `Arc<EventSubscription<AgentEvent>>`. `prompt_records` additionally uses `impl IntoIterator`; whether that generic inherent method can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. Every method also consumes an owned `CancellationToken` class capability, while the class-parameter examples demonstrate only borrowed `&Class`. **UNRESOLVED: not answered by the documentation** for that owned-class input; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. No documented attribute converts an arbitrary existing stream return type. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Agent::{reset_transcript,reset_all}` (`crates/pi-agent-core/src/run.rs:350`) | Not ordinary direct methods: both take `&mut self`, so the existing Agent impl must use `#[export(single_threaded)]`. Their mapped calls can throw once `AgentError` maps, but BoltFFI then disables the `Send + Sync` and mutable-receiver checks and the Swift consumer must serialize every access to that Agent instance. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types] |
| `AgentPhase`, `MessageRole`, `TurnOutcome`, `RunOutcome`, `AgentEvent`, `AgentEventEnvelope` (`crates/pi-agent-core/src/run.rs:30`, `crates/pi-agent-core/src/events.rs:15`) | Intended data enums/structs. `AgentEvent` and the dependent outcomes/envelope are blocked transitively by the canonical message/replay/tool/cost graph, including `Cost`'s undocumented `i128`. `AgentEvent` is non-exhaustive; its target behavior is **UNRESOLVED: not answered by the documentation**. Pages checked: `records.md#enums`, `errors.md#enum-errors`. Payload enums and ordinary structs are otherwise supported. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] |
| `Agent::{active_run_id,phase,last_error}` (`crates/pi-agent-core/src/run.rs:212`) | `active_run_id` and `phase` can be direct only if their returned values are owned; `last_error` returns a reference with a non-static lifetime and is blocked. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AgentControl`, `Agent::control` (`crates/pi-agent-core/src/control.rs:107`, `crates/pi-agent-core/src/run.rs:172`) | Intended direct Rust-owned class and owned class getter. A Rust-backed class may be returned from another class method. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `AgentControl::{steer,follow_up}` (`crates/pi-agent-core/src/control.rs:154`) | Direct async throwing methods after `AgentRecord`, IDs, receipts, and `ControlError` map. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] |
| `AgentControl::{cancel,clear_steering,clear_follow_up,clear_all,set_steering_mode,steering_mode,set_follow_up_mode,follow_up_mode}` (`crates/pi-agent-core/src/control.rs:164`) | Direct synchronous class methods after IDs and queue-mode data map. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] |
| `Agent::{set_steering_mode,steering_mode,set_follow_up_mode,follow_up_mode,clear_steering_queue,clear_follow_up_queue,clear_all_queues}` (`crates/pi-agent-core/src/run.rs:177`) | These signatures use `&self` and otherwise fit documented class methods after their data dependencies map. They live in the same existing impl as mutable methods, so the proposed impl-level attachment is nevertheless `#[export(single_threaded)]`, with Swift responsible for serializing all calls on the instance. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] |
| `Agent::set_tool_execution_mode` (`crates/pi-agent-core/src/run.rs:268`) | Not ordinary direct: it takes `&mut self` and therefore requires the containing Agent impl to use `#[export(single_threaded)]`; if `ToolExecutionMode` and `AgentError` map, the Swift call is synchronous and throwing, but target-side serialization is the consumer's responsibility. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types] |
| `QueueSequence`, `QueueKind`, `QueueReceipt`, `QueueDrainMode` (`crates/pi-agent-core/src/control.rs:19`) | `QueueKind`, `QueueReceipt`, and `QueueDrainMode` are intended direct enum/record data. `QueueSequence` is a tuple newtype; tuple-newtype generation is undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `records.md#enums`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `ControlError` (`crates/pi-agent-core/src/control.rs:68`) | Intended direct error enum; non-exhaustive target behavior is **UNRESOLVED: not answered by the documentation** as for `AgentError`; pages checked: `errors.md#enum-errors`, `records.md#enums`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `DEFAULT_QUEUE_CAPACITY` (`crates/pi-agent-core/src/control.rs:14`) | Direct scalar constant. [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] |
| `ToolSpec`, `ToolCallContext`, `ToolOutput`, `ToolUpdate` (`crates/pi-ai/src/messages.rs:311`, `crates/pi-agent-core/src/tools.rs:32`) | Intended record/enum data, blocked by `serde_json::Value`, `Box<RawValue>`, `GrammarVariants`/`BTreeMap`, and the broader message graph. These types have no documented built-in mappings. The documented nearest alternative is a custom conversion to a supported representation, which violates R2. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types] |
| `ToolOutput::new`, `ToolError::new`, `ToolUpdateError::new` (`crates/pi-agent-core/src/tools.rs:72`, `crates/pi-agent-core/src/tools.rs:143`, `crates/pi-agent-core/src/tools.rs:168`) | Each uses generic conversion input. Whether these generic inherent constructors can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. The concrete result/error values remain candidates for data/error attributes. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ToolError`, `ToolUpdateError` (`crates/pi-agent-core/src/tools.rs:134`, `crates/pi-agent-core/src/tools.rs:161`) | Direct error structs once payload field types map. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] |
| `Tool` (`crates/pi-agent-core/src/tools.rs:201`) | Blocked as written. Its host protocol direction and `Arc` ownership are documented, but `spec()` returns `&ToolSpec` with a non-static lifetime and `execute()` returns `SendBoxFuture` rather than being `async fn`. `execute` also receives an owned `CancellationToken` class; only borrowed class parameters are demonstrated. **UNRESOLVED: not answered by the documentation** for that owned-class input; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. The documented async protocol form requires `#[async_trait]` plus `async fn`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `ToolUpdateSink` (`crates/pi-agent-core/src/tools.rs:184`) | The Swift host must receive and invoke a Rust library implementation. The docs describe a Swift implementation passed into Rust, not a generated proxy for a Rust implementation passed into a Swift callback. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#traits`, `callbacks.md#ownership`, `callbacks.md#how-it-works`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#how-it-works] |
| `ToolArgumentPreparer` (`crates/pi-agent-core/src/tools.rs:242`) | Intended direct host protocol, blocked by borrowed `serde_json::Value` input and undocumented JSON mapping. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types] |
| `ToolExecutionMode`, `ConstrainedSampling`, `ConstrainedSamplingConfig`, `JsonSchemaStrictMode`, `GrammarFormat` (`crates/pi-agent-core/src/tools.rs:21`, `crates/pi-ai/src/messages.rs:334`) | Intended direct data enums/struct; `ConstrainedSamplingConfig` is blocked transitively by its JSON/grammar map branches. Payload enums and structs are documented. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] |
| `GrammarVariants` (`crates/pi-ai/src/messages.rs:418`) | Blocked/undocumented `BTreeMap` alias. The overview lists `HashMap` only as an exportable category; the type-page collections section documents vectors/slices and does not establish a Swift map mapping, `BTreeMap`, or nested-map behavior. The custom-type conversion mechanism is the nearest documented alternative. [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers] |
| `TypedTool<I,F>`, `TypedTool::{new,from_spec,with_execution_mode}` (`crates/pi-agent-core/src/tools.rs:277`) | `TypedTool<I,F>` is blocked by the documented prohibition on generic structs. Whether its generic inherent constructors and methods can export is separately **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ToolRegistry`, `ToolRegistry::{new,register,register_with_argument_preparer,get,is_empty,len}` (`crates/pi-agent-core/src/tools.rs:491`) | Class candidate. `new`, `is_empty`, and `len` are signature candidates; registration is blocked with the current `Tool`/preparer protocols and, because both registration methods take `&mut self`, would require `#[export(single_threaded)]` even after that callback gap is resolved. `get` returns a reference with a non-static lifetime to an `Arc<dyn Tool>` and is blocked. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ContextPolicy`, `MessageProjector`, `ToolPolicy`, `TurnPolicy` (`crates/pi-agent-core/src/policy.rs:116`) | Blocked — explicit boxed-future callback signatures, borrowed contexts, and trait-object members do not match the documented `async fn` callback form. Some contexts are type aliases; whether aliases themselves are exportable is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `types.md#whats-not-supported`, `callbacks.md#async-methods`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AgentStateView`, `PreparedAgentRecords`, `PreparedContext`, `ContextError` (`crates/pi-agent-core/src/policy.rs:16`) | Intended records/error, blocked by borrowed fields and the transitive agent/message graph. Non-static lifetimes are unsupported. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `AgentRunContext<Tools>`, `AgentContext`, `BeforeToolCall<'a,Tools>`, `AfterToolCall<'a,Tools>`, `CompletedTurn<'a,Tools>`, `NextTurn<Tools>` (`crates/pi-agent-core/src/policy.rs:37`) | The generic structs and non-static lifetime-bearing values are blocked. `AgentContext` is a concrete alias whose expansion still contains the blocked graph, but whether a type alias itself can be exported is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `types.md#whats-not-supported`, `records.md#structs`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] |
| `ToolAuthorization`, `ToolOutputPatch` (`crates/pi-agent-core/src/policy.rs:238`) | Intended record/payload-enum data, blocked transitively by tool outputs and JSON values. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `TurnPolicyError` (`crates/pi-agent-core/src/policy.rs:459`) | Intended direct error after payload types map. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `CommittedEventReplay::{new,apply,state,next_sequence,into_state}` (`crates/pi-agent-core/src/replay.rs:16`) | As a class, `new` and `next_sequence` are candidates. `apply(&mut self, &AgentEventEnvelope)` requires `#[export(single_threaded)]`, and its borrowed data-record input is independently **UNRESOLVED: not answered by the documentation**; the input pages demonstrate owned data and borrowed classes, not `&Record`. `state` returns a reference with a non-static lifetime and is blocked; `into_state(self)` is unresolved because consuming class/record methods are not described. As a record impl, no documented `#[skip]` can select only qualifying members. Pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `classes.md#single-threaded-mode`, `classes.md#methods`, `records.md#methods-and-constructors`, `classes.md#skipping-methods`, `types.md#whats-not-supported`. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `replay_committed_events` (`crates/pi-agent-core/src/replay.rs:112`) | Blocked — lifetime-generic borrowed input function. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `committed_record` (`crates/pi-agent-core/src/replay.rs:125`) | Returns owned `Option<AgentRecord>`, but accepts `&AgentEvent`. The function page documents owned data arguments, not borrowed enum inputs. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `functions.md#option`. An owned-input forwarding function would change the native signature and fail R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#option] |
| `TokioAgentHandle`, `TokioAgentHandle::{new,spawn,with_capacities}` (`crates/pi-agent-runtime-tokio/src/lib.rs:158`) | `TokioAgentHandle` is a Send class candidate, but all three constructors consume an owned `Agent`. The class-parameter examples demonstrate `&Class` and class returns, not owned class arguments. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#classes`, `classes.md#constructors`, `classes.md#methods-that-take-or-return-classes`. The `Agent` construction path is independently blocked, and Tokio work requires an active runtime because BoltFFI supplies no executor. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime] |
| `TokioAgentHandle::{prompt_text,continue_run,retry_last_turn,steer,follow_up,cancel,reset_transcript,reset_all,snapshot,wait_for_idle,shutdown}` (`crates/pi-agent-runtime-tokio/src/lib.rs:203`) | Direct async/throwing class methods after their input/result graphs map. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] |
| `TokioAgentHandle::prompt_records` (`crates/pi-agent-runtime-tokio/src/lib.rs:230`) | The method uses `impl IntoIterator`, but the free-function limitation does not establish a rule for generic inherent methods. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `TokioAgentHandle::prompt_text_with_sink`, `subscribe`, `unsubscribe` (`crates/pi-agent-runtime-tokio/src/lib.rs:217`, `crates/pi-agent-runtime-tokio/src/lib.rs:306`) | Intended async methods, blocked by `AgentEventSink`'s non-documented boxed-future callback signature; `unsubscribe` is conditional on `EventSinkId`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `TokioAgentHandle::cancel_now` (`crates/pi-agent-runtime-tokio/src/lib.rs:301`) | Direct synchronous class method after `RunId` maps; this preserves the native re-entrant cancellation operation. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] |
| `TokioAgentHandle::{latest_snapshot,snapshots}` (`crates/pi-agent-runtime-tokio/src/lib.rs:364`) | `latest_snapshot` is a direct owned getter after snapshot mapping. `snapshots` is blocked because `tokio::sync::watch::Receiver` is not a documented type and the method is not an `EventSubscription` stream. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] |
| `TokioAgentRun` (`crates/pi-agent-runtime-tokio/src/lib.rs:126`) | Class candidate: its Tokio receiver fields stay private because only annotated impl methods are exposed, so they do not themselves need a data mapping. The existing impl contains `&mut self` methods and therefore requires `#[export(single_threaded)]`, which disables both default checks and makes Swift-side serialization mandatory; `events` and `outcome` remain signature gaps below. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] |
| `TokioAgentRun::events` (`crates/pi-agent-runtime-tokio/src/lib.rs:133`) | Blocked — returns a Tokio receiver behind a reference with a non-static lifetime, not `Arc<EventSubscription<AgentEvent>>`; both its type boundary and stream signature conflict with the documented mappings. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] |
| `TokioAgentRun::next_event` (`crates/pi-agent-runtime-tokio/src/lib.rs:138`) | Not ordinary direct: this is an async `&mut self` method, so it can be a Swift async pull returning `AgentEvent?` only under `#[export(single_threaded)]` and after the event graph maps. The generated wrapper performs no thread-safety enforcement in that mode; Swift must serialize calls. It does not generate an async sequence. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option] |
| `TokioAgentRun::outcome` (`crates/pi-agent-runtime-tokio/src/lib.rs:143`) | The method is async but consumes `self`. **UNRESOLVED: not answered by the documentation** — consuming class methods are not specified; pages checked: `classes.md#methods`, `classes.md#memory-management`, `async.md#methods`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods] |
| `AgentEventSink` (`crates/pi-agent-runtime-tokio/src/lib.rs:45`) | Blocked — host protocol direction is documented, but `on_event` returns `SendBoxFuture<'static, ()>` instead of being `async fn`; its acknowledgement is part of native ordering and cannot be replaced with an unacknowledged closure. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `EventSinkId` (`crates/pi-agent-runtime-tokio/src/lib.rs:37`) | Opaque tuple-newtype mapping is undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `classes.md#defining-a-class`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `TokioAgentError` (`crates/pi-agent-runtime-tokio/src/lib.rs:70`) | Intended direct error enum; non-exhaustive target behavior is **UNRESOLVED: not answered by the documentation**; pages checked: `errors.md#enum-errors`, `records.md#enums`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `DEFAULT_COMMAND_CAPACITY`, `DEFAULT_EVENT_CAPACITY` (`crates/pi-agent-runtime-tokio/src/lib.rs:30`) | Direct scalar constants. [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] |

### `pi_ai` runtime, models, cancellation, deferred, messages, and events

| Inventory item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `ModelRequest` (`crates/pi-ai/src/runtime.rs:13`) | Intended direct data, blocked transitively by `Context`, `SimpleGenerationOptions`, ordered JSON, and header maps. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `ModelRuntime` (`crates/pi-ai/src/runtime.rs:87`) | Blocked — its method returns `SendBoxFuture<Result<AssistantStream, RequestStartError>>`, not the documented `async fn` trait method form, and it receives an owned `CancellationToken` class for which only borrowed-class parameter examples exist. The normal object is implemented by Rust `Models`, and that library-to-library trait-object route is not covered by the host callback page. **UNRESOLVED: not answered by the documentation** for both library-supplied protocol proxies and owned class inputs; pages checked: `callbacks.md#traits`, `callbacks.md#ownership`, `callbacks.md#async-methods`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `RequestStartErrorKind`, `RequestStartError` (`crates/pi-ai/src/runtime.rs:25`) | Intended direct data enum plus direct error struct. Non-exhaustive enum behavior is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#enums`, `errors.md#enum-errors`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `RequestStartError::{new,with_model}` (`crates/pi-ai/src/runtime.rs:61`) | `new` uses `impl Into<String>`, but the free-function limitation does not establish a rule for generic inherent constructors. **UNRESOLVED: not answered by the documentation** for `new`; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. `with_model(mut self, ...)` is not generic; it is **UNRESOLVED: not answered by the documentation** because consuming record/error methods are not described. The error/data method pages do not provide a selective omission mechanism for a mixed record impl. Pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `errors.md#struct-errors`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] |
| `SendBoxFuture<'a,T>`, `SendBoxStream<'a,T>` (`crates/pi-ai/src/async_types.rs:11`) | Their expansions contain arbitrary future/stream trait objects and non-static lifetime parameters, which are blocked. Whether generic type aliases themselves can export is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `types.md#whats-not-supported`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] |
| `AssistantStream::{new,from_boxed,is_terminated}` (`crates/pi-ai/src/streaming.rs:1900`) | Blocked as a generated Swift stream: it wraps `SendBoxStream`, not `EventSubscription<AssistantEvent>`. `from_boxed` accepts that independently blocked alias. `new<S>` is a generic inherent constructor, whose exportability is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. `is_terminated` alone is a synchronous method candidate if the wrapper class can otherwise export. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `Models`, `Models::builder`, `ModelsBuilder` (`crates/pi-ai/src/models.rs:49`, `crates/pi-ai/src/models.rs:1445`) | `Models` and `ModelsBuilder` are class candidates: class structs and their private fields stay private, so private trait-object storage is not a field-mapping gap. Concretely, `Models` uses `Arc<ModelsInner>` and `RwLock`, and every stored callback capability shown in `ModelsInner` is declared `Send + Sync + 'static` (`crates/pi-ai/src/models.rs:49`, `crates/pi-ai/src/middleware.rs:337`, `crates/pi-ai/src/auth.rs:259`, `crates/pi-ai/src/catalog.rs:269`). That source shape is aligned with the default class check; the actual annotated build must still pass BoltFFI's compile-time `Send + Sync` assertion, and every public signature must map. The inherent `Models::builder` can return another Rust-backed class in principle; the consuming builder methods remain unresolved below. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Models::default` (`crates/pi-ai/src/models.rs:99`) | This is defined by `impl Default for Models`, not an inherent static method. The class and record pages demonstrate annotated inherent impls only. **UNRESOLVED: not answered by the documentation**; pages checked: `classes.md#defining-a-class`, `classes.md#static-methods`, `records.md#methods-and-constructors`, `records.md#static-methods`. Adding an inherent forwarding constructor would be the nearest shape shown by the class page, but it changes the native API and fails R2; without `Models::default`, R1 is incomplete. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods] |
| `ModelsBuilder::{provider,build}` (`crates/pi-ai/src/models.rs:1501`) | `provider(mut self, ...)` and `build(self)` are **UNRESOLVED: not answered by the documentation** because both consume the existing builder. `provider` also accepts an owned `ProviderRegistration` class candidate, while the documented class parameters are borrowed; its callback graph is independently blocked. Pages checked: `classes.md#methods`, `records.md#methods-and-constructors`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `ModelsBuilder::{credential_store,auth_context,models_store,model_override_store}` (`crates/pi-ai/src/models.rs:1476`) | Every method consumes and returns the builder, which is **UNRESOLVED: not answered by the documentation**; their trait parameters are independently blocked by boxed-future, borrowed, consuming, and nested-trait-object signatures described below. Pages checked: `classes.md#methods`, `callbacks.md#async-methods`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `ModelsBuilder::{header_transform,payload_transform,erased_payload_transform,response_observer,attempt_middleware}` (`crates/pi-ai/src/models.rs:1507`) | Every method consumes and returns the builder, which is undocumented; the callback graphs are also blocked by boxed futures and borrowed/mutable-borrowed values. `PayloadTransform<A>` is a generic callback trait whose signature uses types selected through the associated types on `A: ApiFamily`; the generic trait shape and associated types are documented as unsupported. `payload_transform<A>` is itself a generic inherent method; its generic-method status is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. Consuming class methods are also **UNRESOLVED: not answered by the documentation**; pages checked: `classes.md#methods`, `callbacks.md#limitations`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ProviderRegistration`, `ProviderRegistration::builder`, `ProviderRegistrationBuilder::{new,display_name,base_url,headers,auth,catalog,catalog_source,models,filter_models,api,retry_policy,retry_classifier,build}` (`crates/pi-ai/src/provider.rs:2320`) | `ProviderRegistration::builder` and builder `new` use generic ID inputs; whether these generic inherent constructors can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. Every other listed builder operation consumes the existing builder and is **UNRESOLVED: not answered by the documentation**; maps, protocol values, arbitrary trait-object closure aliases, and nested async callbacks independently block several signatures. Pages checked: `classes.md#methods`, `records.md#methods-and-constructors`, `types.md#whats-not-supported`, `callbacks.md#async-methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `ProviderRegistrationError` (`crates/pi-ai/src/provider.rs:2529`) | Direct error enum after payload fields map. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `Models::{stream_simple,stream_simple_with_auth}` (`crates/pi-ai/src/models.rs:762`) | Blocked — these are functions returning futures rather than declared `async fn`, their success value is `AssistantStream` rather than the documented generated stream shape, and they receive an owned `CancellationToken` class input whose direction is undocumented. The docs map declared async functions, not arbitrary future-returning functions. **UNRESOLVED: not answered by the documentation** for the owned class argument; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#standalone-functions] [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Models::{stream_api,stream_api_with_request_options}` (`crates/pi-ai/src/models.rs:785`) | Both methods are generic over `A: ApiFamily`. `ApiFamily` itself is a non-generic trait with five associated types; those associated types are documented as unsupported. Their future/stream returns also fail the documented forms. Whether the generic inherent methods themselves can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. Both also take an owned `CancellationToken` class, an input direction not demonstrated by the docs. **UNRESOLVED: not answered by the documentation** for that input; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Models::{fetch_deferred,fetch_deferred_with_auth,cancel_deferred,cancel_deferred_with_auth}` (`crates/pi-ai/src/models.rs:827`) | Blocked as written because they return future values rather than being declared `async fn`; their input/output graphs contain the blocked `DeferredHandle` and assistant message graph; and all take an owned `CancellationToken` class. **UNRESOLVED: not answered by the documentation** for that owned-class input; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Models::{providers,provider,models,filter_models,model}` (`crates/pi-ai/src/models.rs:115`) | Not direct as a group. Borrowed returns carry non-static lifetimes and are blocked; owned snapshots are blocked by undocumented `Arc<[T]>` aliases and the descriptor graph; `provider(&ProviderId)` and `model(&ModelRef)` have borrowed data inputs not demonstrated by the docs; `filter_models` has its own callback/collection graph. **UNRESOLVED: not answered by the documentation** for the borrowed inputs; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `types.md#collections`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] |
| `Models::{check_auth,get_available,credential_info,resolve_auth,login,logout}` (`crates/pi-ai/src/models.rs:167`) | Blocked where the functions return futures rather than being `async fn`, accept the nested auth callback graph, or take the owned `CancellationToken` class used by every listed call. **UNRESOLVED: not answered by the documentation** for that owned-class input; pages checked: `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. Any actual exported `async fn -> Result` would map to Swift `async throws`. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `Models::credential_store` (`crates/pi-ai/src/models.rs:259`) | Blocked — returns a trait-object capability behind a reference with a non-static lifetime. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `Models::{catalog_snapshot,catalog_layers,set_provider,remove_provider,clear_providers,refresh_host_overrides,set_runtime_overrides,clear_runtime_overrides,refresh}` (`crates/pi-ai/src/models.rs:370`) | Not direct as a group: catalog data, callback registration, and future-return forms block individual methods. A generic conversion input appears on this method family; whether such a generic inherent method can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. `catalog_snapshot(&ProviderId)` and `catalog_layers(&ProviderId)` additionally have borrowed data inputs, and `refresh` takes an owned `CancellationToken` class; neither direction is demonstrated. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `ProviderSnapshot`, `ModelSnapshot` (`crates/pi-ai/src/models.rs:42`) | Blocked/undocumented `Arc<[T]>` aliases. **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#collections`, `custom-types.md#containers`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers] |
| `AuthCheck`, `AuthSource`, `ResolvedAuth`, `AuthResolutionPurpose` (`crates/pi-ai/src/provider.rs:211`) | Intended direct auth data structs/enums; blocked transitively where credential/header fields use unsupported graph leaves. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `ResolveAuthRequest`, `ApiKeyResolveRequest` (`crates/pi-ai/src/provider.rs:146`, `crates/pi-ai/src/auth.rs:1372`) | Intended callback records, blocked by nested `Arc<dyn AuthContext>` and borrowed/capability fields. Arbitrary trait objects are outside the general supported type set. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `SecretString`, `ApiKeyCredential`, `OAuthCredential`, `ProviderOAuthExtra`, `Credential`, `CredentialType`, `CredentialInfo`, `AuthResolutionOverrides` (`crates/pi-ai/src/auth.rs:27`, `crates/pi-ai/src/auth.rs:1347`) | Intended records/payload enums. The graph is blocked by opaque JSON and map types; tuple-newtype mapping for `SecretString` is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. `Duration` itself has a built-in Swift `TimeInterval` mapping. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#duration] |
| `CredentialStore`, `CredentialLease`, and `CredentialStore::{read,list,acquire_lease}` / `CredentialLease::{current,replace,commit}` (`crates/pi-ai/src/auth.rs:247`) | Blocked — explicit boxed futures, a getter returning a reference with a non-static lifetime, a consuming async method, and a nested returned `Box<dyn CredentialLease>` exceed the documented host-protocol examples. The docs require actual async trait methods for generated async requirements. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AuthContext`, `AuthResolver`, `AuthInteraction`, `RedirectReceiver` (`crates/pi-ai/src/auth.rs:682`, `crates/pi-ai/src/provider.rs:248`) | Blocked — boxed-future methods, a URI reference return with a non-static lifetime, consuming receiver, and a protocol method returning another boxed protocol. Nested returned protocol objects and consuming protocol methods are undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#ownership`, `callbacks.md#async-methods`, `classes.md#memory-management`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AuthHostCapabilities`, `AuthPrompt`, `AuthAnswer`, `AuthSelectOption`, `AuthEvent`, `AuthInfoLink`, `RedirectReceiverRequest`, `RedirectStrategy`, `AuthHtmlPage`, `RedirectArrival`, `RedirectStrategyDescription` (`crates/pi-ai/src/auth.rs:922`) | Intended direct record/enum data graph; `url::Url` maps directly, while newtypes and any opaque JSON branches retain their separate gaps. Payload enums and nested records are documented constructs. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#url] |
| `AuthChallengeId` (`crates/pi-ai/src/ids.rs:99`) | Open string tuple-newtype mapping is undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `AuthInteractionError`, `AuthError`, `StoreError` (`crates/pi-ai/src/auth.rs:1043`, `crates/pi-ai/src/catalog.rs:1481`) | Intended direct error enums/struct; blocked only by unsupported payload leaves, with non-exhaustive behavior unresolved. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] |
| `ProviderAuthResolver`, `EnvironmentApiKeyAuth`, `AnonymousAuthResolver`, `EmptyAuthContext`, `MapAuthContext`, `InMemoryCredentialStore`, `SystemAuthClock` (`crates/pi-ai/src/auth.rs:1728`) | Intended direct Rust-owned classes for library implementations. Methods involving blocked protocols, maps, non-static references, or future returns remain blocked. Rust-backed class mapping itself is documented. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ApiKeyAuth`, `AuthClock` (`crates/pi-ai/src/auth.rs:1384`, `crates/pi-ai/src/auth.rs:1695`) | Intended host protocols; blocked where methods use boxed futures or borrowed request graphs. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `ProviderAuthResolver::{new,with_clock}`, `EnvironmentApiKeyAuth::new`, `MapAuthContext::{new,with_file}`, `InMemoryCredentialStore::new` (`crates/pi-ai/src/auth.rs:1749`, `crates/pi-ai/src/auth.rs:817`) | The `new` items are class-constructor candidates only where map and protocol inputs map. Constructors using `impl Into`/`impl IntoIterator` are generic inherent constructors, whose mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. `ProviderAuthResolver::with_clock(mut self, ...)` and `MapAuthContext::with_file(mut self, ...)` consume the existing class and are **UNRESOLVED: not answered by the documentation**; pages checked: `classes.md#methods`, `records.md#methods-and-constructors`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] |
| `OAuthAuth` (`crates/pi-ai/src/auth.rs:1643`) | Blocked — its async operations return `SendBoxFuture` instead of using actual `async fn`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `PkcePair`, `OAuthAuthorizationInput` (`crates/pi-ai/src/oauth.rs:22`, `crates/pi-ai/src/oauth.rs:97`) | Direct data records with supported optional/string fields. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#optional-fields] |
| `generate_pkce`, `generate_oauth_state`, `validate_oauth_state`, `parse_oauth_authorization_input` (`crates/pi-ai/src/oauth.rs:30`) | Direct free-function candidates after their errors map: the generators take no inputs, while validation/parsing use the documented `&str` parameter shape; fallible functions become throwing calls. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#result] |
| `redirect_strategy_supported` (`crates/pi-ai/src/auth.rs:2377`) | Not direct: both parameters are borrowed data values, `&RedirectStrategy` and `&AuthHostCapabilities`. The function docs demonstrate owned structs/enums and borrowed classes, not borrowed record/enum inputs. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`. An owned-input forwarder would require a new signature and fail R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] |
| `select_first_valid<T,LeftFactory,LeftFuture,RightFactory,RightFuture>`, `OAuthDeviceCodePollResult<T>`, `OAuthDeviceCodePoll<T>`, `OAuthDeviceCodePollOptions<T>`, `OAuthDeviceCodePollOptions::new`, `poll_oauth_device_code_flow<T>` (`crates/pi-ai/src/oauth.rs:155`) | `select_first_valid` and `poll_oauth_device_code_flow` are generic free functions and are blocked. `OAuthDeviceCodePoll<T>` is a generic callback trait and is blocked; its boxed-future method is independently incompatible with the documented async-trait form. `OAuthDeviceCodePollOptions<T>` is a generic struct and is blocked. `OAuthDeviceCodePollResult<T>` is a generic enum, which the type-page prohibition on generic structs does not cover; its mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#whats-not-supported`, `records.md#enums-with-associated-data`. `OAuthDeviceCodePollOptions::new` is a generic inherent constructor, whose mapping is also **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] |
| `OAuthDeviceCodeRuntime` (`crates/pi-ai/src/oauth.rs:268`) | Intended host protocol, blocked by its explicit async carrier signatures if not declared as actual `async fn`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `SystemOAuthDeviceCodeRuntime` (`crates/pi-ai/src/oauth.rs:295`) | Intended direct Rust-backed class, with individual methods blocked where future carriers are unsupported. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] |
| `create_supported_redirect_receiver` (`crates/pi-ai/src/auth.rs:2393`) | Although declared async, it returns `Box<dyn RedirectReceiver>` and accepts both a host-protocol object and an owned `CancellationToken` class. Returned host-protocol objects and owned class inputs are undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#ownership`, `async.md#standalone-functions`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#standalone-functions] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `std::time::Duration`, `url::Url` (`crates/pi-ai/src/oauth.rs:13`) | Direct built-in custom types mapping to Swift `TimeInterval` and `URL`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#duration] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#url] |
| `ProviderDescriptor` (`crates/pi-ai/src/provider.rs:44`) | Intended direct data; `url::Url` is built in, but `HeaderMapSpec` blocks the full record. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#url] |
| `HeaderMapSpec`, `http::HeaderMap` (`crates/pi-ai/src/model.rs:17`, `crates/pi-ai/src/provider.rs:100`) | Blocked/undocumented map types. The overview merely lists `HashMap` as exportable; the type-page collections section documents vectors/slices and establishes no Swift mapping for `HashMap`, `BTreeMap`, nested maps, or `http::HeaderMap`. Custom conversion is the nearest documented mechanism and violates R2. [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] |
| `ModelCatalog`, `ModelCatalogSource`, `ModelsStore`, `ModelOverrideStore` (`crates/pi-ai/src/provider.rs:353`, `crates/pi-ai/src/catalog.rs:243`) | Intended host protocols, blocked by boxed-future methods, returns with non-static lifetimes, callback capability fields, or unsupported collection graphs. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `StaticModelCatalog`, `ProviderCatalogState` (`crates/pi-ai/src/provider.rs:388`, `crates/pi-ai/src/catalog.rs:538`) | Intended Rust-backed classes/data, blocked transitively by descriptor snapshots, dynamic-source protocols, and unsupported collections. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `CatalogFetchContext`, `CatalogCandidate`, `CatalogSnapshot`, `PersistedCatalogSnapshot`, `ProviderCatalogLayers`, `ModelOverride`, `ModelOverrideAction`, `ModelOverridePatch`, `RefreshRequest`, `RefreshReport`, `ProviderRefreshResult` (`crates/pi-ai/src/catalog.rs:231`) | Intended data records/payload enums; blocked wherever the descriptor, JSON, map, set, `Arc` slice, or callback graph is nested. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `CatalogError`, `CatalogErrorReport`, `StoreError`, `OverrideError` (`crates/pi-ai/src/catalog.rs:1421`) | Intended direct error/report structs after payload graphs map. `CatalogErrorReport` is data rather than necessarily a thrown error and should use `#[data]`; error items use `#[error]`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] |
| `ModelAvailabilityFilter` (`crates/pi-ai/src/provider.rs:2309`) | Its expansion is an arbitrary `Fn` trait object and is blocked by the general type restriction. Whether a type alias itself can export is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `types.md#whats-not-supported`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] |
| `ChatApi`, `RetryClassifier`, `ErasedApiHandler`, `HttpTransport`, `RetrySleeper` (`crates/pi-ai/src/provider.rs:877`, `crates/pi-ai/src/retry.rs:273`) | Intended host protocols, blocked by boxed-future/stream return carriers, borrowed contexts, and nested callback objects. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ResolvedApiRequest`, `ResolvedDeferredRequest`, `ApiCallOptions<'a>`, `ApiExecutionContext<'a>`, `DeferredExecutionContext<'a>`, `ProviderResponseStream` (`crates/pi-ai/src/provider.rs:559`) | Intended request/context records, blocked by non-static borrowed fields, trait-object slices, and the boxed response-stream alias. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `AiError`, `AiErrorKind`, `RetryPolicy`, `AttemptFailure`, `RetryDecision` (`crates/pi-ai/src/provider.rs:448`, `crates/pi-ai/src/retry.rs:15`) | Intended direct error/data graph after nested values map. Payload error enums are documented. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `HttpChatApi`, `HttpChatApi::{new,with_retry_sleeper}` (`crates/pi-ai/src/provider.rs:959`) | `HttpChatApi` is a Rust-backed class candidate. `new` is blocked by callback-object inputs. `with_retry_sleeper(mut self, ...)` consumes the existing class and is **UNRESOLVED: not answered by the documentation** in addition to its callback blocker; pages checked: `classes.md#methods`, `records.md#methods-and-constructors`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#storing-traits] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] |
| `HttpRequest`, `HttpBody`, `HttpResponse`, `TransportError` (`crates/pi-ai/src/middleware.rs:20`) | Request/response/error records are intended data/error, but `HttpBody` and response body are boxed streams rather than `EventSubscription`; header and HTTP types also lack mappings. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] |
| `ModelDescriptor`, `CommonModelDescriptor`, `ModalityCapabilities`, `ModelLimits`, `ModelPricing`, `TokenPriceRates`, `RequestWidePriceTier`, `CacheWriteRetentionPricing` (`crates/pi-ai/src/model.rs:29`, `crates/pi-ai/src/usage.rs:202`) | Intended nested records. The complete graph is blocked by `BTreeSet`, `BTreeMap`, tuple newtypes, ordered/raw JSON, generic thinking maps, and `MoneyRate`'s `i128` value. The primitive quick reference ends at 64-bit integers, so `i128` is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `records.md#nested-structs`, `custom-types.md#choosing-an-approach`. A custom conversion to a supported representation is the documented nearest alternative, but requires new conversion code and violates R2. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] |
| `Modality`, `MaxTokensField`, `OpenAiThinkingFormat`, `ThinkingTokenBudgetField`, `CacheControlFormat`, `DeferredToolsMode`, `SessionAffinityFormat`, `ChatTemplateVariableName`, `OpenRouterDataCollection`, `NullableString`, `OpenAiThinkingValue`, `AnthropicThinkingValue`, `AnthropicEffort` (`crates/pi-ai/src/model.rs:78`) | Direct data enums; payload-bearing siblings remain subject to their payload blockers. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `MoneyRate`, `MoneyRate::{new,cost_for_tokens}` (`crates/pi-ai/src/usage.rs:130`) | The tuple-newtype form and its `i128` representation/return are undocumented; the primitive quick reference stops at `i64`/`u64`. Whole owner-defined `MoneyRate` could use `#[custom_ffi]` plus `CustomFfiConvertible`, but that requires conversion code and violates R2. It does not resolve the naked `Result<i128, _>` return of `cost_for_tokens`; the custom-type page documents converting a whole owner-defined type, not overriding a bare primitive signature. `cost_for_tokens(self, ...)` also consumes the record. **UNRESOLVED: not answered by the documentation** for naked `i128` returns and consuming methods; pages checked: `records.md#structs`, `records.md#methods-and-constructors`, `types.md#quick-reference`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. Preserving the method requires a signature/wrapper change unless documentation adds a primitive mapping. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `ApiModelConfig`, `ApiModelConfig::api_id`, `OpenAiCompletionsModelConfig`, `OpenAiCompletionsCompat`, `OpenAiResponsesModelConfig`, `OpenAiResponsesCompat`, `AnthropicMessagesModelConfig`, `AnthropicMessagesCompat`, `AnthropicFallbackModel`, `GoogleModelConfig`, `BedrockModelConfig`, `BedrockCompat`, `MistralModelConfig` (`crates/pi-ai/src/model.rs:112`) | Intended direct payload enum/records and owned method; blocked transitively by generic `ThinkingLevelMap<T>`, `ExtensionMap`, ordered JSON, and map aliases. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `ChatTemplateValues`, `ExtensionMap` (`crates/pi-ai/src/model.rs:13`, `crates/pi-ai/src/model.rs:25`) | Blocked — `IndexMap` and `BTreeMap` aliases are not in the documented collection set. The nearest documented alternative is a custom type conversion. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers] |
| `ChatTemplateKwargValue`, `ChatTemplateVariable`, `OpenRouterRouting`, `OpenRouterSort`, `OpenRouterSortOptions`, `OpenRouterMaxPrice`, `JsonNumberOrString`, `OpenRouterMetricPreference`, `OpenRouterPercentiles`, `VercelGatewayRouting` (`crates/pi-ai/src/model.rs:269`) | Intended direct data enum/record graph, blocked where `serde_json::Number`, ordered maps, or nested blocked values occur. Payload enums and nested records are supported as constructs. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `CustomApiModelConfig`, `VersionedExtension` (`crates/pi-ai/src/model.rs:741`, `crates/pi-ai/src/model.rs:919`) | Intended records, blocked by exact `Box<RawValue>`. The documented nearest alternative is custom conversion to a supported representation; implementing it is extra code and invalid conversion panics. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors] |
| `ThinkingLevelMap<T>`, `LevelSupport<T>`, `ReasoningLevelResolution<T>`, `ThinkingLevelMap::{get,resolve}` (`crates/pi-ai/src/model.rs:763`) | `ThinkingLevelMap<T>` and `ReasoningLevelResolution<T>` are generic structs and are blocked. `get` additionally returns a reference with a non-static lifetime. `LevelSupport<T>` is a generic enum, and the documentation's prohibition names generic structs rather than generic enums; its mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#whats-not-supported`, `records.md#enums-with-associated-data`. Whether `get` and `resolve` can export as inherent methods on a generic type is also **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `OrderedJsonObject`, `OrderedJsonArray`, `OrderedJsonValue`, `OrderedJsonString` (`crates/pi-ai/src/json_compat.rs:24`) | Blocked/undocumented recursive ordered JSON graph. The nested-collections section demonstrates nested `Vec`, not maps; neither it nor the overview's bare `HashMap` listing documents recursive records, `IndexMap`, nested maps, or exact UTF-16 string storage. **UNRESOLVED: not answered by the documentation**; pages checked: `overview.md#what-you-can-export`, `types.md#nested-collections`, `records.md#nested-structs`, `custom-types.md#containers`. The inventory also identifies generic inherent mutators/iterators on this graph; their method mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#nested-collections] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ProviderId`, `ModelId`, `ApiId`, `ExtensionId`, `ModelRef` (`crates/pi-ai/src/ids.rs:58`) | `ModelRef` is intended direct data after its ID fields map. The open IDs are tuple newtypes whose mapping is unresolved; their getters return references with non-static lifetimes and are blocked. Their `impl Into` constructors are generic inherent constructors, whose mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ModelPricing::{rates_for,calculate_cost,calculate_cost_with_multiplier}` (`crates/pi-ai/src/usage.rs:213`) | All three accept borrowed `&Usage`, a borrowed-data input not demonstrated by the docs; `rates_for` also returns `&TokenPriceRates` with a non-static lifetime and is blocked. `calculate_cost` returns owner-defined `Cost`, whose `micros` field is `i128`; a whole-type custom conversion is documented but adds code and fails R2. `calculate_cost_with_multiplier` instead has naked `i128` numerator/denominator arguments. The custom-type page documents whole external or owner-defined type conversion, not overriding bare primitive parameters. **UNRESOLVED: not answered by the documentation** for borrowed record inputs and naked `i128` arguments; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `types.md#whats-not-supported`, `types.md#quick-reference`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. The naked arguments require a signature/wrapper change unless documentation adds a mapping. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `Currency`, `CacheWriteRetention`, `Cost`, `CostArithmeticError` (`crates/pi-ai/src/usage.rs:88`) | `CacheWriteRetention` is a direct enum candidate and `CostArithmeticError` a direct error candidate. `Currency` is a tuple newtype whose mapping is unresolved. `Cost` is not direct because `micros` is `i128`, absent from the primitive quick reference. Whole owner-defined `Cost` could use `#[custom_ffi]` plus `CustomFfiConvertible`, which adds conversion code and fails R2; this does not establish any bare-primitive signature mapping. **UNRESOLVED: not answered by the documentation** for direct `i128` fields; pages checked: `types.md#quick-reference`, `records.md#structs`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `url::Url`, `serde_json::Number`, `serde_json::value::RawValue`, `indexmap::IndexMap`, `BTreeMap`, `BTreeSet` (`crates/pi-ai/src/model.rs:4`) | `url::Url` maps directly to Swift `URL`. The other five have no documented built-in mapping. The overview's bare `HashMap` listing does not establish mappings for other map/set types or nested maps, the collections section covers vectors/slices, and `HashSet` alone is explicitly listed as unsupported. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#url] [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `HeaderTransform`, `HeaderTransformContext`, `MiddlewareError` (`crates/pi-ai/src/middleware.rs:325`) | Intended host protocol, callback record, and error. Blocked by boxed-future method, borrowed context, and header-map graph. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `PayloadTransform<A>`, `PayloadTransformContext<'a,A>`, `PayloadTransformResult<T>` (`crates/pi-ai/src/middleware.rs:358`) | `PayloadTransform<A>` is a generic trait with associated API-family types and is blocked; its boxed-future method is independently incompatible with the documented async-trait form. `PayloadTransformContext<'a,A>` is a generic, lifetime-bearing struct and is blocked. `PayloadTransformResult<T>` is a generic enum, which the generic-struct prohibition does not cover; its mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#whats-not-supported`, `records.md#enums-with-associated-data`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `ErasedPayloadTransform`, `ErasedPayloadContext`, `ProviderPayload`, `PayloadTransformDisposition` (`crates/pi-ai/src/middleware.rs:440`) | Intended protocol and payload records/enums, blocked by boxed-future methods, borrowed contexts, arbitrary erased payload storage, and JSON types. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ResponseObserver`, `ResponseObservationContext`, `ProviderResponseMetadata` (`crates/pi-ai/src/middleware.rs:686`) | Intended protocol/data, blocked by boxed-future/borrowed context and header metadata. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `AttemptMiddleware`, `HttpRequest` (`crates/pi-ai/src/middleware.rs:742`) | Blocked — callback mutably borrows a request and returns a boxed future; the documented async protocol form is an actual `async fn`, and non-static borrowing is unsupported. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ApiFamily` (`crates/pi-ai/src/options.rs:368`) | Blocked — this is a non-generic trait with five associated types, and associated types are documented as unsupported. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] |
| `TypedModelDescriptor<A>`, `SimpleLoweringContext<'a,A>`, `EncodeContext<'a,A>` (`crates/pi-ai/src/options.rs:316`) | Blocked — generic/lifetime-bearing structs. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `ErasedApiFullOptions`, `ErasedApiFullOptions::{new,downcast_ref}` (`crates/pi-ai/src/options.rs:410`) | The record is blocked because it stores an arbitrary `Any` trait object; `downcast_ref` is independently blocked by its reference return with a non-static lifetime. Whether the generic inherent `new<A>` and `downcast_ref<A>` methods can export is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `ApiOptionsInput<A>`, `ApiOptionsInput::from_sources` (`crates/pi-ai/src/options.rs:525`) | This is a generic data enum over associated API-family values. The type-page prohibition names generic structs rather than generic enums, and neither it nor the record page answers this shape. `from_sources` is an inherent method on that generic enum, and the free-function page does not answer generic inherent methods. **UNRESOLVED: not answered by the documentation** for both items; pages checked: `types.md#whats-not-supported`, `records.md#enums-with-associated-data`, `records.md#methods-and-constructors`, `classes.md#methods`, `functions.md#limitations`. The associated values depend on `ApiFamily`; `ApiFamily` is non-generic, but its associated types are independently unsupported. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] |
| `ApiRequestOptions`, `ErasedApiOptionsPatch` (`crates/pi-ai/src/options.rs:450`, `crates/pi-ai/src/options.rs:294`) | Intended direct data; blocked by `HeaderMapSpec` and `RawValue` respectively. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `LoweringError`, `EncodeError` (`crates/pi-ai/src/options.rs:654`) | Intended direct error enums; non-exhaustive target behavior is unresolved. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `SimpleGenerationOptions` (`crates/pi-ai/src/options.rs:561`) | Intended data, blocked by recursive ordered JSON, `HeaderMapSpec`, raw API patch, and ID newtypes. Tuple-newtype mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. Listed primitive, optional, `Vec`, and `Duration` fields themselves are documented. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#duration] |
| `StreamTransport`, `ReasoningLevel`, `ReasoningFallback`, `CacheRetention`, `ToolChoice`, `DeferredSubmission`, `DeferredWindow` (`crates/pi-ai/src/options.rs:19`, `crates/pi-ai/src/deferred.rs:102`) | Direct data enums. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `ReasoningLevel::resolve_extended`, `ThinkingBudgets::budget_for`, `DeferredSubmission::is_enabled` (`crates/pi-ai/src/options.rs:41`, `crates/pi-ai/src/deferred.rs:130`) | `ReasoningLevel::resolve_extended(self, ...)` consumes the existing enum and is **UNRESOLVED: not answered by the documentation**. `ThinkingBudgets::budget_for(&self, ...)` and `DeferredSubmission::is_enabled(&self)` are documented record-method shapes after their complete signatures map. Pages checked for the consuming method: `records.md#methods-and-constructors`, `classes.md#methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] |
| `ThinkingBudgets` (`crates/pi-ai/src/options.rs:88`) | Direct data record with optional integer fields. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#optional-fields] |
| `ApiRequestOptions::from` (`crates/pi-ai/src/options.rs:469`) | This method is defined by `impl From<&SimpleGenerationOptions> for ApiRequestOptions`, not an inherent static data method, and its parameter is borrowed data. The docs demonstrate annotated inherent record impls and owned data arguments, but neither trait-implementation methods nor `&Record` inputs. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `records.md#static-methods`, `functions.md#structs-and-enums`, `functions.md#classes`. An inherent owned-input forwarder is the nearest documented shape, but it changes the native API and fails R1/R2. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] |

#### Concrete API-family types

The marker types are useful to Rust only as generic arguments. Every marker —
`OpenAiCompletions`, `OpenAiResponses`, `OpenAiCodexResponses`,
`AnthropicMessages`, `GoogleGenerativeAi`, `GoogleVertex`,
`BedrockConverseStream`, `MistralConversations`,
`pi_ai_openai::AzureOpenAiResponses`, and
`pi_ai_pi_messages::PiMessages` — is blocked at the Swift call boundary because
the call methods are generic over `A: ApiFamily` and use `A::Compat`,
`A::ModelConfig`, `A::FullOptions`, `A::OptionsPatch`, and `A::WireRequest`. `ApiFamily`
is non-generic, but its five associated types are a documented unsupported
trait shape. Whether the generic inherent `Models` methods themselves can
export is
**UNRESOLVED: not answered by the documentation**; pages checked:
`records.md#methods-and-constructors`, `classes.md#methods`, and
`functions.md#limitations`.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]

| Inventory configuration item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `OpenAiCompletionsOptions`, `OpenAiCompletionsToolChoice`, `OpenAiAllowedToolsMode`, `OpenAiReasoningPlan`, `OpenAiReasoningMode`, `OpenAiReasoningEffortProvenance`, `OpenAiReasoningTokenBudget`, `OpenAiCompletionsSimplePatch` (`crates/pi-ai/src/openai_completions.rs:41`) | Intended direct records/payload enums; blocked where ordered JSON appears. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `OpenAiResponsesOptions`, `OpenAiResponsesReasoningSummary`, `OpenAiResponsesSimplePatch` (`crates/pi-ai/src/openai_responses.rs:102`) | Intended direct record/enum graph; full options are blocked by ordered JSON values, while the summary and simple patch are direct if all scalar fields remain supported. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `OpenAiCodexResponsesOptions`, `OpenAiCodexReasoningSummary`, `OpenAiTextVerbosity`, `OpenAiCodexToolChoice`, `OpenAiCodexResponsesSimplePatch` (`crates/pi-ai/src/openai_responses.rs:124`) | Direct records/enums conditional on ID/newtype leaves. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `AnthropicOptions`, `AnthropicThinking`, `AnthropicThinkingDisplay`, `AnthropicToolChoice`, `AnthropicSimplePatch` (`crates/pi-ai/src/anthropic_messages.rs:37`) | Direct records/payload enums conditional on identifier and nested-option mappings. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `GoogleOptions`, `GoogleVertexOptions`, `GoogleThinkingOptions`, `GoogleThinkingLevel`, `GoogleToolChoice`, `GoogleSimplePatch`, `GoogleCompat` (`crates/pi-ai/src/google.rs:32`) | Direct records/enums; the empty compatibility record is also a record construct, though zero-field target representation is not shown. **UNRESOLVED: not answered by the documentation** for zero-field structs; pages checked: `records.md#structs`, `records.md#enums`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `BedrockOptions`, `BedrockToolChoice`, `BedrockThinkingDisplay`, `BedrockSimplePatch` (`crates/pi-ai/src/bedrock.rs:55`) | Intended records/enums, blocked by `IndexMap<String,String>`, the inventory-excluded scratch field still present in the native `BedrockOptions` struct, and `SecretString`. The `SecretString` newtype mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `records.md#enums`, `types.md#records`. Attribute-only export cannot omit a public field while preserving the same native data record. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs-with-strings-or-collections] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `MistralOptions`, `MistralToolChoice`, `MistralSimplePatch`, `MistralCompat` (`crates/pi-ai/src/mistral.rs:102`) | Record/enum candidates conditional on IDs. The empty-record and newtype representations are **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `pi_ai_openai::AzureOpenAiResponsesModelConfig`, `pi_ai_openai::azure_model_config`, `AzureOpenAiResponsesOptions`, `AzureOpenAiResponsesSimplePatch` (`providers/pi-ai-openai/src/azure.rs:18`) | Records/patch are intended data but inherit ordered/raw JSON blockers. `azure_model_config(&CustomApiModelConfig)` is not direct even after that graph maps because its argument is a borrowed data record. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `functions.md#result`. An owned-input forwarding function would change the native signature and fail R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#result] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| Azure nested `OpenAiResponsesCompat`, `SessionAffinityFormat`, `ExtensionMap`, `VersionedExtension`, `OrderedJsonObject`, `OrderedJsonString`, `OrderedJsonArray`, `OrderedJsonValue` (`crates/pi-ai/src/model.rs:600`) | Enum/record constructs are intended data, but the graph is blocked by `BTreeMap`, exact raw JSON, recursive ordered JSON, and exact UTF-16 string storage. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `pi_ai_pi_messages::PiMessagesCompat`, `PiMessagesOptions`, `PiMessagesSimplePatch`, `PiMessagesToolChoice` (`providers/pi-ai-pi-messages/src/wire.rs:19`) | Intended records/enums; the other three are candidates conditional on IDs/nested values. `PiMessagesCompat`'s zero-field representation is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| Pi Messages `WireRequest` graph: `CustomApiModelConfig`, `OrderedJsonObject`, `OrderedJsonString`, `OrderedJsonArray`, `OrderedJsonValue` (`crates/pi-ai/src/model.rs:741`) | Blocked by raw and recursive ordered JSON. [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] |
| Shared `ApiRequestOptions`, `StreamTransport`, `CacheRetention`, `ReasoningLevel`, `ThinkingBudgets`, `SecretString` (`crates/pi-ai/src/options.rs:19`, `crates/pi-ai/src/auth.rs:27`) | Enum and ordinary-record members are candidates; `ApiRequestOptions` is blocked by `HeaderMapSpec`. `SecretString` tuple-newtype mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| Shared `OrderedJsonObject`, `OrderedJsonArray`, `OrderedJsonValue`, `indexmap::IndexMap<String,String>` (`crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/bedrock.rs:166`) | Blocked/undocumented recursive and ordered-map types. The overview lists `HashMap` without a target mapping, while the detailed collections page documents vectors/slices only; neither page authorizes `IndexMap` or nested map graphs. [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] |

#### Cancellation and deferred values

| Inventory item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `CancellationToken`, `CancellationToken::new`, `CancellationToken::{cancel,is_cancelled,check,child}` (`crates/pi-ai/src/cancellation.rs:28`) | Intended direct thread-safe class and synchronous methods; `check` becomes throwing after `CancellationError` maps, and `child` returns another class. The class docs support class-returning methods. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types] |
| `CancellationToken::cancelled`, `Cancelled<'a>` (`crates/pi-ai/src/cancellation.rs:81`) | Blocked — returns a future borrowing the token rather than being an exported `async fn`, and the future struct carries a non-static lifetime. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `CancellationError` (`crates/pi-ai/src/cancellation.rs:12`) | Error-struct candidate, but its zero-field target representation is **UNRESOLVED: not answered by the documentation**; pages checked: `errors.md#struct-errors`, `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `DeferredHandle`, `DeferredHandle::{new,model_ref}` (`crates/pi-ai/src/deferred.rs:19`) | Intended direct durable record, but the provider `data: Option<serde_json::Value>` field blocks round-trip fidelity; `model_ref` returns a newly assembled owned value. `new` uses generic `impl Into` inputs, but the free-function limitation does not establish a rule for generic inherent constructors. **UNRESOLVED: not answered by the documentation** for `new`; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. The nearest documented alternative for the provider data is a custom conversion, forbidden by R2. [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `DeferredCapabilities`, `DeferredWindow`, `DeferredSubmission`, `DeferredFetchOptions`, `DeferredCancelOptions` (`crates/pi-ai/src/deferred.rs:73`) | Intended direct records/enums; concrete struct/enum members map, but the cancel-options alias is only direct if its aliased type is supported. Type-alias preservation is undocumented. **UNRESOLVED: not answered by the documentation**; pages checked: `types.md#quick-reference`, `records.md#structs`, `records.md#enums`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `DeferredModelRuntime` (`crates/pi-ai/src/deferred.rs:195`) | Blocked — library-implemented supertrait with boxed-future methods. The documented async trait mapping requires actual `async fn`, and library implementations exported as target protocol proxies are not documented. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |

#### Canonical message, assistant-event, identity, usage, and replay graph

| Inventory item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `Message`, `UserMessage`, `AssistantMessage`, `ToolResultMessage`, `ContentBlock`, `ToolCall`, `ToolResultContent`, `Conversation`, `Context` (`crates/pi-ai/src/messages.rs:32`) | Intended direct data records and payload enums. The graph is blocked by `serde_json::Value`, `serde_json::Number`, `Box<RawValue>`, `BTreeMap`, `DeferredHandle`, and dependent newtypes. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `Message::id`, `ContentBlock::id` (`crates/pi-ai/src/messages.rs:105`, `crates/pi-ai/src/messages.rs:288`) | Blocked — their returned references carry non-static lifetimes. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `Conversation::new`, `Context::new` (`crates/pi-ai/src/messages.rs:455`, `crates/pi-ai/src/messages.rs:480`) | Intended direct data constructors, conditional on the complete message/tool graph. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] |
| `AssistantFinish`, `AssistantFinishReason`, `ContentBlockKind`, `ReplayDataOperation`, `CancellationReason`, `AssistantMessageSnapshot` (`crates/pi-ai/src/messages.rs:492`, `crates/pi-ai/src/streaming.rs:347`) | Intended record/enum data. Byte branches map `Vec<u8>` to Swift `Data`; snapshots/cancellation remain blocked by their broader nested graphs. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#bytes] |
| `CancellationReason::{new,with_request_id}` (`crates/pi-ai/src/streaming.rs:588`) | Both methods use generic conversion inputs, but the free-function limitation does not establish a rule for generic inherent methods or constructors. **UNRESOLVED: not answered by the documentation** for that genericity; pages checked: `records.md#constructors`, `records.md#methods-and-constructors`, `classes.md#constructors`, `classes.md#methods`, `functions.md#limitations`. `with_request_id(mut self, ...)` also consumes an existing record, which is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `PublicError` (`crates/pi-ai/src/messages.rs:521`) | Direct **data struct**, not `#[error]`: it is persisted operational data nested inside messages/outcomes rather than the `E` in these method `Result` signatures. Struct records map to Swift value structs. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] |
| `DiagnosticErrorCode`, `DiagnosticErrorInfo`, `AssistantMessageDiagnostic` (`crates/pi-ai/src/messages.rs:178`) | Intended direct data, blocked by `serde_json::Number`, `BTreeMap<String, Value>`, and JSON detail fields. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `AssistantEvent`, `AssistantEvent::{is_terminal,terminal_message}` (`crates/pi-ai/src/streaming.rs:360`) | Intended payload enum, blocked by the canonical value graph. In its existing mixed record impl, `is_terminal(&self)` has a documented receiver shape but `terminal_message(&self) -> Option<&AssistantMessage>` returns a reference with a non-static lifetime and is blocked. The docs do not authorize class-only `#[skip]` inside `#[data(impl)]`, so attribute-only integration cannot selectively expose `is_terminal`. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. Target handling of the enum's `#[non_exhaustive]` marker is also **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#enums`, `errors.md#enum-errors`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] |
| `ProviderId`, `ModelId`, `ApiId`, `MessageId`, `ContentBlockId`, `ToolCallId`, `ReplayItemId`, `ReplayKind`, `ExtensionId`, `RunId` (`crates/pi-ai/src/ids.rs:58`) | Blocked pending tuple-newtype support. Their `as_str` methods return references with non-static lifetimes and are blocked. Their generic `new` constructors are **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. Their `into_inner(self)` methods consume existing record values and are **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `records.md#methods-and-constructors`, `classes.md#methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] |
| `ModelRef`, `ModelRef::new` (`crates/pi-ai/src/ids.rs:105`) | Intended direct two-field record. Its `impl Into` constructor is a generic inherent constructor, whose mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `Timestamp`, `Timestamp::{from_unix_millis,unix_millis}` (`crates/pi-ai/src/ids.rs:133`) | Tuple-newtype representation is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. The concrete scalar constructor/getter otherwise have documented record-method shapes. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] |
| `UsageSource`, `Usage`, `Usage::{zero,request_input_tokens,total_tokens}`, `Cost` (`crates/pi-ai/src/usage.rs:13`) | `UsageSource` and `Usage` have record/enum shapes, and `zero` has a documented static-method shape. Whole owner-defined `Usage` or `Cost` could be mapped with `#[custom_ffi]` plus `CustomFfiConvertible`, but that adds conversion code and violates R2. It does not override the naked `u128` returns from `Usage::{request_input_tokens,total_tokens}`; the primitive quick reference stops at 64 bits and the custom-type page covers whole owner-defined types. **UNRESOLVED: not answered by the documentation** for naked `u128` returns. Preserving those methods requires signature/wrapper changes unless documentation adds a primitive mapping. Because the `Usage` impl mixes `zero` with undocumented-width returns and record `#[skip]` is not documented, it is not attribute-only exportable. Pages checked: `types.md#quick-reference`, `records.md#structs`, `records.md#enums`, `records.md#methods-and-constructors`, `classes.md#skipping-methods`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `Currency`, `Currency::{new,as_str,usd}` (`crates/pi-ai/src/usage.rs:88`) | Tuple-newtype mapping is unresolved; `as_str` returns a reference with a non-static lifetime and is blocked, while `usd` alone has a documented static-method shape. `new` is a generic inherent constructor, whose mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. The docs do not authorize `#[skip]` in `#[data(impl)]`, so the mixed impl cannot selectively expose `usd` under R2. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `ReplayEnvelope`, `ReplayScope`, `ReplayItem`, `ReplayTarget`, `ReplayApplicability`, `ReplayCompleteness`, `OpaquePayload` (`crates/pi-ai/src/replay.rs:13`) | Intended lossless records/payload enums. `OpaquePayload` must remain three distinct `Utf8(String)`, `Bytes(Vec<u8>)`, and `JsonBytes(Vec<u8>)` cases; `Vec<u8>` maps to Swift `Data`. This preserves replay opacity rather than decoding provider payloads. The graph is blocked by helpers returning references with non-static lifetimes and ID tuple newtypes. `ReplayEnvelope::is_complete_and_applicable` and `ReplayItem::is_complete_and_applicable` additionally take borrowed replay records; the docs demonstrate owned data inputs, not `&Record`. **UNRESOLVED: not answered by the documentation** for tuple newtypes and borrowed data inputs; pages checked: `records.md#structs`, `types.md#records`, `functions.md#structs-and-enums`, `functions.md#classes`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#bytes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] |
| `OpaquePayloadEncodingError` (`crates/pi-ai/src/replay.rs:371`) | Error-struct candidate. If its native shape is zero-field, the target representation is **UNRESOLVED: not answered by the documentation**; pages checked: `errors.md#struct-errors`, `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `ModelFingerprint`, `ReplayDropReason`, `HandoffChange`, `HandoffReport` (`crates/pi-ai/src/handoff.rs:17`) | Intended data graph. `ReplayDropReason` is an open tuple newtype whose mapping is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| Repeated option graph: `ReasoningLevel`, `ReasoningFallback`, `ThinkingBudgets`, `CacheRetention`, `ToolChoice`, `StreamTransport`, `ApiRequestOptions`, `SimpleGenerationOptions`, `OrderedJsonObject`, `OrderedJsonString`, `OrderedJsonValue`, `OrderedJsonArray`, `HeaderMapSpec`, `ErasedApiOptionsPatch`, `VersionedExtension` (`crates/pi-ai/src/options.rs:19`) | Same mapping as above: leaf enums/ordinary record direct; encompassing options blocked by ordered/raw JSON and map types. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] |

#### Assistant and agent stream semantics

If the native signatures already returned `Arc<EventSubscription<AssistantEvent>>`
and `Arc<EventSubscription<AgentEvent>>`, async mode would generate Swift
`AsyncStream<AssistantEvent>` and `AsyncStream<AgentEvent>` respectively.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#async-mode]
They do not, so the mapping is blocked under R2. In addition, the documented
producer does **not** apply backpressure: it never blocks and drops each new
event delivered to a subscriber whose ring buffer is full, with 256 items as
the default capacity. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
That documented loss policy conflicts with the lossless ordered assistant-event
boundary and with acknowledged agent-sink barriers; choosing a larger buffer
does not turn the policy into backpressure.

Documented termination is subscription completion when the producer
unsubscribes, and consumer cancellation occurs when the Swift task is cancelled
or iteration breaks. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stopping-streams]
The docs generate nonthrowing `AsyncStream<T>`, not a documented
`AsyncThrowingStream<T, Error>`; this repository's terminal assistant and agent
failures can remain value events (`AssistantEvent`/`RunOutcome`), while
pre-stream `RequestStartError` remains a throwing start failure only if an
exportable async start method exists. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#async-mode]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]

### `pi_agent_session` durable storage and state

| Inventory item and Rust source | BoltFFI-to-Swift mapping and verdict |
|---|---|
| `SessionStorage`, `SessionStorage::{metadata,load_state,append,log,repair_tail}` (`crates/pi-agent-session/src/storage.rs:18`) | Blocked — the host-implemented trait's five operations return `SendBoxFuture` rather than being actual `async fn`. Its value inputs/results are additionally blocked by the session graph. The documented nearest alternative is an exported `#[async_trait]` trait with declared async methods, which would change the existing contract. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `SessionRepository`, `SessionRepository::{create,open,fork,list}` (`crates/pi-agent-session/src/storage.rs:70`) | Blocked — boxed-future methods, `open(&SessionId)`/`fork(&SessionId, ...)` borrowed data inputs, and methods returning `Arc<dyn SessionStorage>`. The docs do not cover borrowed record inputs or a host protocol returning another host protocol implementation for later Rust use. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#ownership`, `callbacks.md#async-methods`, `functions.md#structs-and-enums`, `functions.md#classes`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| `InMemorySessionStorage::{new,state_snapshot,append_batch,metadata_snapshot,log_snapshot}` (`crates/pi-agent-session/src/storage.rs:126`) | Intended direct Rust-backed class and synchronous/throwing methods after the owned record/`Vec` input and result graphs map. The constructor/class method shapes and `Vec` arguments are documented; this row does not rely on borrowed record inputs. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#vec] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types] |
| `InMemorySessionRepository::new` (`crates/pi-agent-session/src/storage.rs:307`) | Intended direct Rust-backed class constructor; methods returning `Arc<dyn SessionStorage>` remain blocked as above. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] |
| `SessionErrorKind`, `SessionError`, `SessionReductionError` (`crates/pi-agent-session/src/error.rs:10`) | Intended direct error enum/struct/enum. Non-exhaustive reducer-error behavior is **UNRESOLVED: not answered by the documentation**; pages checked: `errors.md#enum-errors`, `records.md#enums`. Any unsupported payload leaf also blocks that variant graph. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `SessionId`, `EntryId`, `LaneName`, `OperationRecordId`, `Sequence` (`crates/pi-agent-session/src/ids.rs:58`) | Blocked pending tuple-newtype support. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#structs`, `types.md#records`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `SessionMetadata`, `AppendReceipt`, `TailRepairReport`, `SessionHeader`, `SessionEnvironmentMetadata`, `CreateSessionRequest`, `ForkRequest`, `SessionQuery`, `ForkPosition` (`crates/pi-agent-session/src/types.rs:30`) | Intended direct records/enum; blocked transitively wherever ID newtypes, metadata `VersionedExtension`/JSON, message graph, maps, or other blocked session values appear. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `SessionReducer::{apply,state}` (`crates/pi-agent-session/src/reducer.rs:13`) | The library implements this trait rather than the Swift host. Generated target proxies for library implementations are undocumented, `apply(&SessionMutation)` has an undocumented borrowed-data input, and `state` returns a reference with a non-static lifetime. **UNRESOLVED: not answered by the documentation** for the proxy/input direction; pages checked: `callbacks.md#traits`, `functions.md#structs-and-enums`, `functions.md#classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `SessionState`, `SessionState::new` (`crates/pi-agent-session/src/reducer.rs:23`) | Intended direct record or Rust-backed class. Its large field graph contains maps and blocked canonical values, so the complete data record does not map attribute-only. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `SessionState::replay` (`crates/pi-agent-session/src/reducer.rs:77`) | This associated constructor uses `impl IntoIterator`, but the free-function limitation does not establish a rule for generic inherent constructors. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#constructors`, `classes.md#constructors`, `functions.md#limitations`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] |
| `SessionState::{sequence,next_sequence}` (`crates/pi-agent-session/src/reducer.rs:88`) | Direct synchronous methods after `Sequence` maps. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods] |
| `SessionState::{entry,entries_in_sequence_order,records_in_sequence_order,lanes,lane_leaf,log,name,label,labels,stats,scan_branch_leaf_to_root,scan_branch_root_to_leaf,open_operations,recovery_decision}` (`crates/pi-agent-session/src/reducer.rs:100`) | Blocked where they return references or borrowed collections with non-static lifetimes, or values with blocked graph leaves. `entry`, `lane_leaf`, `label`, both scans, `open_operations`, and `recovery_decision` also accept borrowed ID/enum data values; that input direction is undocumented even where a method returns an owned value. **UNRESOLVED: not answered by the documentation** for those borrowed inputs; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `types.md#collections`, `types.md#whats-not-supported`. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| `SessionState::create_fork_mutations` (`crates/pi-agent-session/src/reducer.rs:254`) | Its `Vec<SessionMutation>` and `Result` return shapes are documented, but the input is `&ForkPosition`. The docs demonstrate owned data arguments, not borrowed enum inputs. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#structs-and-enums`, `functions.md#classes`, `functions.md#vec`, `functions.md#result`. An owned-input forwarding method would change the native signature and fail R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#vec] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#result] |
| `SessionMutation`, `SessionMutation::sequence` (`crates/pi-agent-session/src/types.rs:772`) | Intended direct payload enum and owned scalar/newtype getter, blocked transitively by the entry/record/fact graph and ID newtypes. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `EntryBase`, `SessionEntry`, `SessionEntry::{base,id,sequence,parent_id,with_base}` (`crates/pi-agent-session/src/types.rs:84`) | Intended record/payload enum. The accessors return references with non-static lifetimes and are blocked. `with_base(mut self, ...)` consumes the existing record and is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`. Because the existing record impl contains only unsupported or unresolved methods and record-method omission is undocumented, `#[data(impl)]` cannot expose this impl attribute-only. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `ProvisionedEntry`, `ProvisionedEntry::{id,materialize}` (`crates/pi-agent-session/src/types.rs:223`) | Intended payload enum. `id(&self)` returns a reference with a non-static lifetime and is blocked. `materialize(self, ...)` consumes the existing record and is **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#methods`. The mixed impl has no documented record `#[skip]`, so it is not attribute-only exportable. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `OperationRecordBase`, `OperationIntent`, `OperationOutcome`, `OperationStep`, `CompactionReason`, `ToolCallIdentity`, `ToolReplayPolicy`, `pi_agent_session::QueueKind` (`crates/pi-agent-session/src/types.rs:374`) | Intended direct records/enums; blocked only through their transitive canonical message, deferred, ID, and JSON leaves. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] |
| `UsageAttribution`, `SignedUsageAdjustment`, `UsageAttribution::run_id` (`crates/pi-agent-session/src/types.rs:495`) | `UsageAttribution` is a payload-enum candidate, but `run_id(&self)` returns a reference with a non-static lifetime and is blocked. `SignedUsageAdjustment` is not direct because its five fields are `i128`, absent from the primitive quick reference. A whole owner-defined `SignedUsageAdjustment` can use `#[custom_ffi]` plus `CustomFfiConvertible`, but that adds conversion code and violates R2; this row does not infer support for naked `i128` signatures. **UNRESOLVED: not answered by the documentation** for direct `i128` record fields; pages checked: `types.md#quick-reference`, `records.md#structs`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `OperationRecord`, `OperationRecord::{base,sequence,lane,run_id}` (`crates/pi-agent-session/src/types.rs:587`) | Intended payload enum, blocked by its nested operation/session graph. `base`, `lane`, and `run_id` return references with non-static lifetimes and are blocked; `sequence` is an owned-newtype candidate only after `Sequence` maps. Because the docs do not authorize class-only `#[skip]` inside the mixed `#[data(impl)]`, attribute-only integration cannot selectively expose `sequence`. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `SessionFact`, `LaneState`, `RecoveryDecision`, `SessionStats` (`crates/pi-agent-session/src/types.rs:754`) | Intended enum/records; blocked transitively by IDs, public error, maps, fixed-point currency values, and operation records. `SessionStats` is independently not direct because its cached/uncached/total token fields and map values are `i128`, absent from the primitive quick reference; the map type is also undocumented beyond the overview's bare `HashMap` listing. A whole owner-defined `SessionStats` could use `#[custom_ffi]` plus `CustomFfiConvertible`, but that adds conversion code and violates R2; it does not establish support for bare `i128` method signatures elsewhere. **UNRESOLVED: not answered by the documentation** for direct `i128` record fields and the map graph; pages checked: `types.md#quick-reference`, `records.md#enums`, `overview.md#what-you-can-export`, `types.md#collections`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| `SESSION_HEADER_SCHEMA_VERSION`, `SESSION_STATE_SCHEMA_VERSION`, `SESSION_METADATA_SCHEMA_VERSION`, `APPEND_RECEIPT_SCHEMA_VERSION`, `TAIL_REPAIR_REPORT_SCHEMA_VERSION` (`crates/pi-agent-session/src/types.rs:14`) | Direct scalar constants. [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] |

### Error mapping summary

Use `#[error]` on actual Rust error values that appear as `E` in
`Result<T, E>`: `AgentError`, `ControlError`, `ToolError`, `ToolUpdateError`,
`TokioAgentError`, `RequestStartError`, `ProviderRegistrationError`,
`AuthInteractionError`, `AuthError`, `StoreError`, `CatalogError`,
`OverrideError`, `AiError`, `TransportError`, `MiddlewareError`,
`LoweringError`, `EncodeError`, `CostArithmeticError`, `CancellationError`,
`OpaquePayloadEncodingError`, `ContextError`, `TurnPolicyError`,
`SessionError`, and `SessionReductionError`. Struct and enum errors generate
Swift `Error` values and make `Result` calls throw.
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]

Keep `PublicError`, `CatalogErrorReport`, `DiagnosticErrorInfo`, and any other
persisted/report payload as `#[data]`: they are values inside messages,
outcomes, or reports, not boundary-thrown errors. Struct records generate Swift
value structs. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]

**UNRESOLVED: not answered by the documentation** — whether
`#[non_exhaustive]` Rust errors/enums generate a future-compatible unknown case,
and whether a callback protocol method returning `Result` becomes a throwing
Swift protocol requirement. Pages checked: `errors.md#enum-errors`,
`errors.md#async-errors`, `callbacks.md#traits`, and
`callbacks.md#async-methods`.
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]

## 4. Swift consumer sketches

These sketches deliberately distinguish documented target-language syntax from
what the present Rust signatures can generate. The function page says Rust
function names may be renamed to target conventions and gives the
`get_user`-to-`getUser` example used as the naming convention in these sketches.
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions]

### Run an agent and iterate its event stream

This is the required idiomatic consumer shape. The `for await` loop is exactly
the documented Swift use of a generated `AsyncStream<T>`.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#async-mode]
The `try await` calls use the documented mapping from exported async `Result`
functions to Swift `async throws`.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
The value parameters and event/outcome cases use the documented Swift
struct/payload-enum forms, conditional on their complete graphs mapping.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]

```swift
func runAndObserve(
    handle: TokioAgentHandle,
    prompt: PromptText,
    onEvent: (AgentEvent) -> Void
) async throws -> RunOutcome {
    let run = try await handle.promptText(prompt: prompt)
    for await event in run.events() {
        onEvent(event)
    }
    return try await run.outcome()
}
```

**This sketch is not generated from the current API.** `TokioAgentRun::events`
returns `&mut tokio::sync::mpsc::Receiver<AgentEvent>` at
`crates/pi-agent-runtime-tokio/src/lib.rs:133`, not the required
`Arc<EventSubscription<AgentEvent>>`; `outcome(self)` is consuming, whose class
mapping is unresolved. The attribute-only native fallback is a pull loop over
the existing async `next_event` method, provided the event graph maps and the
containing impl is marked `#[export(single_threaded)]`. That mode disables the
thread-safety and mutable-receiver checks and makes the Swift caller responsible
for serializing access to the run object.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]

```swift
func pullRunEvents(
    run: TokioAgentRun,
    onEvent: (AgentEvent) -> Void
) async {
    // This task is the sole serialized owner of `run` while pulling events.
    while let event = await run.nextEvent() {
        onEvent(event)
    }
}
```

An exported Rust async method becomes a Swift async method and `Option<T>`
becomes Swift optional, authorizing the language features in the pull sketch.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option]
The pull sketch does not satisfy R3 because it is not a generated async
sequence; generated asynchronous streams use the documented `#[ffi_stream]`
shape instead.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]

### Implement a `Tool` in Swift

BoltFFI documents generating a Swift protocol from an exported Rust trait and
permits the target language to implement it; an actual Rust `async fn` trait
method becomes a Swift async protocol requirement.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
If `Tool` had the documented owned/async signature, a host implementation would
have this shape, with the complex values injected so the sketch does not invent
a JSON representation:
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]

```swift
final class FixedOutputTool: Tool {
    private let fixedSpec: ToolSpec
    private let fixedOutput: ToolOutput

    init(spec: ToolSpec, output: ToolOutput) {
        self.fixedSpec = spec
        self.fixedOutput = output
    }

    func spec() -> ToolSpec {
        fixedSpec
    }

    func executionMode() -> ToolExecutionMode {
        .sequential
    }

    func execute(
        context: ToolCallContext,
        updates: ToolUpdateSink,
        cancellation: CancellationToken
    ) async -> ToolOutput {
        fixedOutput
    }
}
```

This is the nearest documented protocol sketch, **not a binding for the current
`Tool` trait**. Current `Tool::spec` returns `&ToolSpec`, current
`Tool::execute` returns `SendBoxFuture<Result<ToolOutput, ToolError>>`, and
`ToolUpdateSink` must travel in the undocumented library-implementation-to-host
direction (`crates/pi-agent-core/src/tools.rs:201`). `execute` also takes an
owned `CancellationToken`; the docs show borrowed class parameters, not owned
class arguments. **UNRESOLVED: not answered by the documentation**; pages
checked: `functions.md#classes`,
`classes.md#methods-that-take-or-return-classes`.
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
The documentation does not
say that an async callback-protocol `Result` generates `async throws`, so the
sketch intentionally does not claim Swift `throws` for `execute`.
**UNRESOLVED: not answered by the documentation**; pages checked:
`callbacks.md#async-methods`, `errors.md#async-errors`.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]

### Cancel a run

The native actor cancellation API can be used after a `RunId` is observed:

```swift
func cancelRun(
    handle: TokioAgentHandle,
    runId: RunId
) async throws {
    try await handle.cancel(runId: runId)
}
```

The syntax is authorized if the existing `async fn -> Result` and its ID/error
types map. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
For re-entrant host callback use, the native `handle.cancelNow(runId:)` is the
synchronous class-method candidate at
`crates/pi-agent-runtime-tokio/src/lib.rs:301`.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]

Swift task cancellation is a separate mechanism: cancelling a task that is
awaiting an exported async call cooperatively cancels that Rust future, and
cancelling the task that iterates a generated stream cancels its subscription.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#consumer-side-cancellation]
Neither statement implies that cancelling an arbitrary wait task invokes the
agent's run-ID cancellation contract.

### Resume a deferred run

**Gap verdict: the inventoried ordinary Rust surface has no end-to-end deferred
agent-resumption operation, so this design cannot truthfully present a Swift
resume sketch.** The fetch portion must include the native `ModelRef` argument.
Expressed with the intended Swift spelling, that portion would be:

```swift
// Signature illustration only: fetching is not agent resumption.
let fetchedAssistant = try await models.fetchDeferred(
    model: model,
    handle: handle,
    options: fetchOptions,
    cancellation: cancellation
)
```

This `try await` spelling is only the documented target shape for an exported
`async fn -> Result`; the existing `Models::fetch_deferred` instead returns
`SendBoxFuture`, so even this fetch-only fragment remains blocked as recorded in
the mapping table.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]

The native method requires `model: ModelRef` and returns the first terminal
assistant message observed while polling; it has no `Agent` or
`TokioAgentHandle` argument and does not mutate agent state
(`crates/pi-ai/src/models.rs:827`, `crates/pi-ai/src/models.rs:870`).
The returned message can itself remain pending with finish reason `Deferred`
and the same handle (`crates/pi-ai/src/models.rs:824`).
`TokioAgentHandle::continue_run` accepts no message
(`crates/pi-agent-runtime-tokio/src/lib.rs:243`), and the underlying
`Agent::continue_run` rejects an assistant transcript tail unless steering or
follow-up records are already queued (`crates/pi-agent-core/src/run.rs:320`).
It therefore cannot consume or commit `fetchedAssistant`.

`TokioAgentHandle::prompt_records` is the closest inventoried message-taking
operation (`crates/pi-agent-runtime-tokio/src/lib.rs:230`): its low-level path
commits initial records before preparing a new model context
(`crates/pi-agent-core/src/run.rs:888`, `crates/pi-agent-core/src/run.rs:923`).
That is not the same state-machine path as receiving a terminal assistant from
the runtime, which commits the assistant and then evaluates its tool calls
(`crates/pi-agent-core/src/run.rs:1154`, `crates/pi-agent-core/src/run.rs:1191`).
It also has an `impl IntoIterator` parameter. Whether that generic inherent
method can export is **UNRESOLVED: not answered by the documentation**; pages
checked: `records.md#methods-and-constructors`, `classes.md#methods`, and
`functions.md#limitations`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]
Raw transcript mutation through `Agent::state_mut` is not an exposed
alternative because it returns a mutable reference with a non-static lifetime.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]

The missing native operation must accept the fetched `AssistantMessage`, apply
the agent's existing assistant commit and post-commit behavior, and return the
continued run/events. Adding such an operation is source API work, not a
BoltFFI attribute, so it violates R2 until the owner explicitly permits it.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]

## 5. Gaps and risks

### Requirement-level gaps

| Gap or risk | Consequence and documented nearest alternative |
|---|---|
| **R1 versus R2/R3: both semantic event streams are signature-incompatible.** | `AgentEvent` arrives through a borrowed boxed stream or Tokio receiver, while `AssistantEvent` arrives through `AssistantStream`; none returns `Arc<EventSubscription<T>>`. The nearest documented alternative is a new method backed by `EventSubscription<T>`/`StreamProducer<T>`, but that is adapter code and violates R2. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#creating-streams] |
| **Documented stream loss conflicts with the port's lossless/replay-aware boundary.** | BoltFFI's producer never blocks and drops new events for a full subscriber buffer. The nearest documented tuning is choosing buffer capacity, but no documented setting changes the overflow policy to backpressure or lossless delivery. [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity] |
| **Async callback traits do not have the documented source shape.** | `Tool`, `AgentEventSink`, `ModelRuntime`, policy traits, auth/provider/middleware traits, `SessionStorage`, and `SessionRepository` return `SendBoxFuture`; the docs require `#[async_trait]` plus `async fn`. The nearest alternative is a new adapter trait or changing the trait signature, both forbidden by R2. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| **Documented generic and associated-type blockers.** | The docs explicitly reject generic free functions such as `select_first_valid`, generic structs such as `TypedTool<I,F>` and `ThinkingLevelMap<T>`, generic callback traits such as `PayloadTransform<A>`, and associated types such as the five declared by the non-generic `ApiFamily` trait. The function and type pages document concrete exported functions and concrete types as their respective nearest alternatives; the callback page documents no nearest alternative for generic traits or associated types. Any concrete replacement for those trait shapes would add or alter native API and fail R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] |
| **Generic inherent methods/constructors, generic enums, and generic aliases are undocumented.** | This affects `AgentState::new`, both `prompt_records` methods, tool/error/ID/`DeferredHandle` constructors, `SessionState::replay`, `Models::{stream_api,stream_api_with_request_options}`, `ModelsBuilder::payload_transform`, `OAuthDeviceCodePollResult<T>`, `LevelSupport<T>`, `PayloadTransformResult<T>`, `ApiOptionsInput<A>`, and the box-future/stream aliases. The free-function page does not answer inherent methods, while the type-page prohibition names generic structs rather than generic enums or aliases. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#limitations`, `records.md#constructors`, `records.md#methods-and-constructors`, `records.md#enums-with-associated-data`, `classes.md#constructors`, `classes.md#methods`, `types.md#quick-reference`, `types.md#whats-not-supported`. No attribute-only nearest alternative is documented. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| **Consuming record/class methods are undocumented.** | `CommittedEventReplay::into_state`, `RequestStartError::with_model`, `ReasoningLevel::resolve_extended`, `CancellationReason::with_request_id`, ID `into_inner` methods, `SessionEntry::with_base`, `ProvisionedEntry::materialize`, `TokioAgentRun::outcome`, both builder families, and other `with_*` methods consume an existing `self`. **UNRESOLVED: not answered by the documentation**; the record/class method pages show constructors, static methods, borrowed receivers, and mutable receivers, but no consuming instance method. Pages checked: `records.md#methods-and-constructors`, `classes.md#methods`. No documented attribute-only alternative preserves these exact methods. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] |
| **Trait-implementation methods are undocumented.** | `Models::default` lives in `impl Default for Models`, and `ApiRequestOptions::from` lives in `impl From<&SimpleGenerationOptions> for ApiRequestOptions`. The docs demonstrate annotating inherent record/class impls, not `impl Trait for Type`. **UNRESOLVED: not answered by the documentation**; pages checked: `records.md#methods-and-constructors`, `records.md#static-methods`, `classes.md#defining-a-class`, `classes.md#static-methods`. The nearest documented shape is a new inherent forwarding constructor/static method; that is a code/signature addition, so R1 is incomplete and R2 fails. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods] |
| **Borrowed data-record/enum inputs are undocumented.** | Affected inventoried paths include `ModelRefResolver::resolves`, `CommittedEventReplay::apply`, `committed_record`, `Models::{provider,model,catalog_snapshot,catalog_layers}`, `redirect_strategy_supported`, `ApiRequestOptions::from`, `azure_model_config`, every `ModelPricing` method, `SessionRepository::{open,fork}`, `SessionReducer::apply`, several `SessionState` lookups, and `SessionState::create_fork_mutations`. The functions page demonstrates owned data, `&str`, slices, and `&Class`, but not `&Record`/`&Enum`. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#primitives-and-strings`, `functions.md#structs-and-enums`, `functions.md#slices`, `functions.md#classes`, `records.md#instance-methods`. The nearest documented data input is owned, which requires changed or forwarding signatures and therefore fails R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#slices] [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods] |
| **Owned Rust-class inputs are undocumented.** | `Agent::{new,restore}` consumes `ToolRegistry`, `TokioAgentHandle::{new,spawn,with_capacities}` consumes `Agent`, and the model/tool/agent/auth execution families pass `CancellationToken` by value. The docs demonstrate borrowed class arguments (`&Logger`/`&User`) and class return values, not owned class arguments. **UNRESOLVED: not answered by the documentation**; pages checked: `functions.md#classes`, `classes.md#constructors`, `classes.md#methods-that-take-or-return-classes`. A borrowed-input or handle-forwarder signature would change the ordinary Rust surface and fail R1/R2. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| **Mixed record impls have no documented omission attribute.** | `AssistantEvent`, `Usage`, `Currency`, `SessionEntry`, `ProvisionedEntry`, and `OperationRecord` mix potentially supported methods with references, 128-bit returns, or consuming methods. `#[skip]` is documented for class `#[export]` impls only; `records.md` does not document omission within `#[data(impl)]`. **UNRESOLVED: not answered by the documentation**; the nearest documented class alternative does not authorize record use and therefore cannot satisfy R2 for these impls. Pages checked: `records.md#methods-and-constructors`, `classes.md#skipping-methods`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| **Mutable class methods require unchecked single-threaded mode.** | `Agent::{reset_transcript,reset_all,set_tool_execution_mode}` and `TokioAgentRun::next_event` require `#[export(single_threaded)]`; this disables both the `Send + Sync` and `&mut self` checks and transfers serialization responsibility to Swift. This is attribute-only but introduces an explicit consumer-discipline risk for the main run path. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode] |
| **References and lifetimes.** | Native borrowed getters, mutable configuration accessors, bare-agent stream lifetimes, policy contexts, and session projections cannot cross unchanged because their reference results carry non-static lifetimes. The nearest documented alternative is to return owned data rather than a borrowed reference, which changes signatures/semantics. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] |
| **Unsupported or undocumented payload leaves.** | `serde_json::Value`, `serde_json::Number`, `RawValue`, `IndexMap`, `BTreeMap`, `BTreeSet`, `Arc<[T]>`, `http::HeaderMap`, Tokio `mpsc`/`watch` receivers, arbitrary `Any`, and recursive exact ordered JSON block transitive records. The nearest documented mechanism for otherwise unsupported owned values is `custom_type!` or `CustomFfiConvertible`, which requires conversion code and violates R2; only `url::Url`, `Duration`, and `Vec<u8>` among these adjacent external shapes have explicit built-ins. [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types] |
| **`i128`/`u128` numeric widths are undocumented.** | The quick reference stops at `i64`/`u64`, so none of these widths is direct on the available evidence. For whole owner-defined `Cost`, `MoneyRate`, `Usage`, `SignedUsageAdjustment`, or `SessionStats`, `#[custom_ffi]` plus `CustomFfiConvertible` is a documented whole-type alternative, but it adds conversion code and fails R2. That mechanism does not document overriding naked primitive signatures such as `Usage::{request_input_tokens,total_tokens} -> u128`, `MoneyRate::cost_for_tokens -> Result<i128, _>`, or the raw `i128` multiplier parameters in `ModelPricing::calculate_cost_with_multiplier`. **UNRESOLVED: not answered by the documentation** for bare `i128`/`u128` arguments and returns; pages checked: `types.md#quick-reference`, `custom-types.md#the-customfficonvertible-trait`, `custom-types.md#representation-types`. Those signatures require a wrapper/signature change unless documentation adds primitive support. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| **Nested and reverse-direction trait objects.** | `Models` implements `ModelRuntime` for `Agent`; Rust supplies `ToolUpdateSink` to a Swift `Tool`; repositories return `SessionStorage`; auth interactions return `RedirectReceiver`; credential stores return `CredentialLease`. The docs explain Swift implementations passed into Rust through `Box`/`Arc`, but not Rust implementations presented to Swift as protocol objects or protocols returning protocols. **UNRESOLVED: not answered by the documentation**; pages checked: `callbacks.md#traits`, `callbacks.md#ownership`, `callbacks.md#how-it-works`, `classes.md#methods-that-take-or-return-classes`. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#how-it-works] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| **DeferredHandle round trip.** | Its exact provider data is `Option<serde_json::Value>`; dropping or stringifying it would violate R1. The nearest documented custom conversion requires extra code and can panic on invalid conversion, so there is no attribute-only mapping. [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors] |
| **Deferred fetch has no agent-resume operation.** | `Models::fetch_deferred` requires `ModelRef` and returns an `AssistantMessage` but does not commit it (`crates/pi-ai/src/models.rs:827`). `TokioAgentHandle::continue_run` accepts no message (`crates/pi-agent-runtime-tokio/src/lib.rs:243`); `prompt_records` starts a new prompt run through a generic input rather than applying the existing post-assistant path (`crates/pi-agent-runtime-tokio/src/lib.rs:230`, `crates/pi-agent-core/src/run.rs:888`, `crates/pi-agent-core/src/run.rs:1154`). The nearest documented BoltFFI shape would be an exported async class method over supported record/error types, but no such native method exists, and adding one violates R2. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods] [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] |
| **Cancellation has two layers.** | BoltFFI target-task cancellation cooperatively cancels the exported future, while native `CancellationToken` and actor `cancel(RunId)` are explicit library capabilities. Both must remain distinct. The docs support cooperative target-task cancellation but do not equate it with application cancellation tokens. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation] |
| **Tokio runtime ownership.** | `TokioAgentHandle` requires an active Tokio runtime at `crates/pi-agent-runtime-tokio/src/lib.rs:184`. BoltFFI does not provide an executor and says Tokio-dependent work must have a running Tokio runtime. An attribute-only design has no documented mechanism that creates/enters that runtime for Swift. [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime] |
| **Setup is not attributes only.** | BoltFFI requires dependencies, crate type, `build.rs`, and configuration/codegen steps. These are not item API changes but exceed a literal “attributes only” reading of R2. [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project] [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs] |

### Unresolved documentation questions

Every unresolved point raised in the mapping is collected here:

- **UNRESOLVED: not answered by the documentation** — conditional use through
  `cfg_attr`, optional BoltFFI build dependencies, and feature-dependent
  discovery. Pages checked: `installation.md#add-to-your-project`,
  `installation.md#create-buildrs`, `getting-started.md#write-your-code`,
  `configuration.md#package-identity`.
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
  [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
- **UNRESOLVED: not answered by the documentation** — one generated package
  spanning annotations and re-exports across several workspace crates. Pages
  checked: `configuration.md#package-identity`, `packaging.md#overview`,
  `installation.md#create-buildrs`.
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#overview]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
- **UNRESOLVED: not answered by the documentation** — tuple/unit newtypes,
  zero-field records/unit error structs, and type aliases. Private class fields
  are not included in this gap: the class documentation says the struct stays
  private and only impl methods are exposed. Pages checked:
  `records.md#structs`, `records.md#enums`, `classes.md#defining-a-class`,
  `types.md#records`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
- Associated-data enum payload-field visibility is resolved by the documented
  `LoadState` example: its Rust variant payload fields have no `pub` marker, and
  the generated Swift enum exposes the corresponding associated values.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
- **UNRESOLVED: not answered by the documentation** — whether multiple
  inherent impl blocks for one private Rust-backed class can all be annotated
  and merged into one generated target class. This affects `Agent`. Pages
  checked: `classes.md#defining-a-class`, `classes.md#methods`,
  `classes.md#single-threaded-mode`.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]
- **UNRESOLVED: not answered by the documentation** — target handling of Rust
  `#[non_exhaustive]` records/errors. Pages checked: `records.md#enums`,
  `errors.md#enum-errors`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
- **UNRESOLVED: not answered by the documentation** — exporting methods defined
  only by trait implementations. This blocks `Models::default` from
  `impl Default for Models` and `ApiRequestOptions::from` from
  `impl From<&SimpleGenerationOptions>`. Pages checked:
  `records.md#methods-and-constructors`, `records.md#static-methods`,
  `classes.md#defining-a-class`, `classes.md#static-methods`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#static-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods]
- **UNRESOLVED: not answered by the documentation** — borrowed data-record or
  enum inputs. The docs demonstrate owned data inputs, `&str`, slices, and
  borrowed classes, but not `&Record`/`&Enum`. Pages checked:
  `functions.md#primitives-and-strings`, `functions.md#structs-and-enums`,
  `functions.md#slices`, `functions.md#classes`,
  `records.md#instance-methods`.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#primitives-and-strings]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#structs-and-enums]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#slices]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#instance-methods]
- **UNRESOLVED: not answered by the documentation** — owned Rust-backed class
  arguments. The class-parameter examples use `&Logger`/`&User`; no page shows
  an owned class argument such as native `ToolRegistry`, `Agent`, or
  `CancellationToken`. Pages checked: `functions.md#classes`,
  `classes.md#constructors`, `classes.md#methods-that-take-or-return-classes`.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
- **UNRESOLVED: not answered by the documentation** — consuming `self` record
  or class methods. This includes `CommittedEventReplay::into_state`,
  `RequestStartError::with_model`, `ReasoningLevel::resolve_extended`,
  `CancellationReason::with_request_id`, ID `into_inner`,
  `SessionEntry::with_base`, `ProvisionedEntry::materialize`,
  `TokioAgentRun::outcome`, the `ModelsBuilder` and
  `ProviderRegistrationBuilder` transitions, other consuming `with_*` methods,
  and consuming boxed protocol methods. Pages checked:
  `records.md#methods-and-constructors`, `classes.md#methods`,
  `classes.md#memory-management`, `callbacks.md#ownership`,
  `async.md#methods`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods]
- **UNRESOLVED: not answered by the documentation** — selectively omitting a
  method from `#[data(impl)]`. The docs show `#[skip]` only for class
  `#[export]` impls, so mixed record impls cannot be selectively exported on the
  available evidence. Pages checked: `records.md#methods-and-constructors`,
  `classes.md#skipping-methods`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
- **UNRESOLVED: not answered by the documentation** — whether `Result` on an
  async callback-trait method becomes a throwing Swift protocol requirement.
  Pages checked: `callbacks.md#async-methods`, `errors.md#async-errors`.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]
- **UNRESOLVED: not answered by the documentation** — Rust-implemented traits
  exported as Swift protocol proxy objects, nested protocol return values, and
  library-to-host callback capability arguments. Pages checked:
  `callbacks.md#traits`, `callbacks.md#ownership`,
  `callbacks.md#how-it-works`, `classes.md#methods-that-take-or-return-classes`.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#how-it-works]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
- **UNRESOLVED: not answered by the documentation** — a lossless/backpressured
  stream overflow mode, stream terminal errors, or direct adaptation of
  `futures_core::Stream`/Tokio receivers. Pages checked:
  `streaming.md#the-ffi_stream-attribute`, `streaming.md#buffer-capacity`,
  `streaming.md#stopping-streams`, `experimental.md#feature-details`.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stopping-streams]
  [https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#feature-details]
- **UNRESOLVED: not answered by the documentation** — mappings for
  `serde_json::{Value,Number,value::RawValue}`, `IndexMap`, `BTreeMap`,
  `BTreeSet`, `Arc<[T]>`, `http::HeaderMap`, Tokio receivers, arbitrary `Any`,
  recursive/exact ordered JSON, and the Swift mapping/nesting behavior for the
  `HashMap` merely listed in the overview. Pages checked:
  `overview.md#what-you-can-export`, `types.md#quick-reference`,
  `types.md#collections`, `types.md#whats-not-supported`,
  `types.md#built-in-custom-types`, `custom-types.md#representation-types`,
  `custom-types.md#containers`.
  [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#containers]
- **UNRESOLVED: not answered by the documentation** — `i128` and `u128` as
  direct record leaves, arguments, or return values. The primitive quick
  reference ends at `i64`/`u64`. The custom-type page documents conversion of a
  whole external or owner-defined type, so it can be considered for whole
  `Cost`, `MoneyRate`, `Usage`, `SignedUsageAdjustment`, or `SessionStats` only
  with extra conversion code; it does not answer bare primitive parameters or
  returns in `Usage`, `MoneyRate`, and pricing methods. Those naked signatures
  require a wrapper/signature change unless documentation adds support. Pages
  checked: `types.md#quick-reference`,
  `custom-types.md#the-custom_type-macro`,
  `custom-types.md#the-customfficonvertible-trait`,
  `custom-types.md#representation-types`.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-custom_type-macro]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]

### Existing `bindings/pi-ffi` UniFFI layer

The existing binding is a separate unpublished `pi-ffi` crate, already built as
`lib`/`cdylib`/`staticlib` and depending on UniFFI
(`bindings/pi-ffi/Cargo.toml:1`, `bindings/pi-ffi/Cargo.toml:11`,
`bindings/pi-ffi/Cargo.toml:22`). It installs UniFFI scaffolding at
`bindings/pi-ffi/src/lib.rs:41`, exposes JSON/configuration envelopes beginning
at `bindings/pi-ffi/src/lib.rs:43`, and checks in generated Swift at
`bindings/pi-ffi/generated/swift/PiFFI.swift:1`.

The safe migration position is **coexistence first**: keep `pi-ffi` and its
module/artifact unchanged while a differently named BoltFFI Swift package proves
leaf types and individual calls. This avoids symbol/module collisions and keeps
the existing lossless acknowledged event queue, documented in project code at
`bindings/pi-ffi/src/lib.rs:49`, available during evaluation. No snapshot page
describes BoltFFI/UniFFI coexistence or migration. **UNRESOLVED: not answered by
the documentation**; pages checked: `packaging.md#apple-packaging`,
`configuration.md#swift-module-name`, `configuration.md#swiftpm-layouts`.
[https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#apple-packaging]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swift-module-name]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swiftpm-layouts]

BoltFFI cannot replace UniFFI for the R1 boundary until the stream, callback,
generic, JSON/ordered-data, reverse-trait, and runtime gaps have documented or
owner-approved solutions. A replacement decision before those gates would
silently exchange the native API for a smaller surface, contrary to R1.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]

## 6. Phased implementation plan

No phase publishes a crate, and no phase deletes or mutates the existing
UniFFI artifact. Each phase is a separate stop/go gate; a failing acceptance
test prevents expansion to the next phase.

1. **Resolve the architecture gate before implementation.** Obtain an owner
   ruling that either preserves R2 and accepts that the project is blocked, or
   permits precisely enumerated adapter methods/types for streams, callbacks,
   concrete generic instantiations, JSON/order-preserving values, trait-object
   direction, 128-bit numerics, consuming methods, mixed record impls, and Tokio
   runtime ownership. The ruling must also cover a first-class native
   deferred-fetch-to-agent-resumption operation, methods defined in trait impls,
   borrowed record/enum inputs, and owned Rust-class inputs. Also obtain
   documentation answers for conditional attributes, multi-crate discovery,
   and the intended Swift serialization discipline for
   `#[export(single_threaded)]`. Acceptance: a written
   ruling accounts for every requirement-level gap in section 5, and there are
   no implicit envelope substitutions.
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]

2. **One-crate leaf smoke test.** In an isolated implementation change, add the
   documented dependency/build-script/staticlib setup and annotate only leaf
   values such as `PromptImage`, `PkcePair`, `OAuthAuthorizationInput`, one
   scalar constant, one fallible free function, and one error. The documented
   setup uses the dependency, crate type, build generation, and `boltffi check`.
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
   Acceptance: `boltffi check`, `boltffi generate swift`, Rust tests, and a
   Swift XCTest compile/run all pass with the feature both enabled and disabled;
   the generated calls use typed records and Swift errors rather than JSON.

3. **Identity and lossless value graph.** Resolve tuple-newtype behavior and the
   undocumented `i128`/`u128` boundary before attempting `MoneyRate`, `Cost`,
   usage totals, pricing, or session statistics; then map IDs, `ModelRef`,
   timestamp, usage/cost, `PublicError`, replay, handoff, content, messages,
   assistant events, and agent events from the leaves upward. The documented
   custom-conversion mechanism applies to a whole owner-defined type only if the
   owner permits the new conversion code that R2 currently forbids. It does not
   resolve naked `i128`/`u128` arguments or returns; those methods remain gated
   on a documented primitive mapping or an approved signature/wrapper change.
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
   [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait]
   [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]
   Use payload-enum records and Swift `Data` for byte vectors, which are
   documented mappings. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#bytes]
   Acceptance: Swift-to-Rust-to-Swift round trips preserve every enum variant,
   every ID, `OpaquePayload::Utf8`, `OpaquePayload::Bytes`,
   `OpaquePayload::JsonBytes`, complete `ReplayEnvelope` ordering/applicability,
   `DeferredHandle` provider data byte-for-byte or structurally exactly as its
   native type requires, and the minimum/maximum supported signed and unsigned
   128-bit fixture values without truncation.

4. **Rust-owned classes and ordinary async calls.** Map `CancellationToken`,
   `AgentControl`, `TokioAgentHandle`, `Models`, reducers, and in-memory session
   classes only where signatures are already supported. Do not map a call that
   accepts `ToolRegistry`, `Agent`, `CancellationToken`, or another Rust class by
   value until the owned-class-input gap is resolved; likewise do not claim
   `Models::default` until trait-implementation methods are resolved. Do not map
   consuming builder transitions until the documentation gap has an approved resolution.
   Map `Agent` mutable methods and `TokioAgentRun::next_event` only under the
   documented `#[export(single_threaded)]` mode and an explicit Swift
   serialization wrapper/test. Exported async methods become Swift async and
   async `Result` becomes `async throws`.
   [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#static-methods]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]
   [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
   Acceptance: Swift constructs each mapped class, calls synchronous and async
   methods, observes exact typed errors, cancels a run by `RunId`, and proves the
   Tokio runtime is active for every Tokio-dependent call. A concurrency stress
   test also proves that Swift serializes every call to each single-threaded
   `Agent`/`TokioAgentRun` wrapper; no test treats `next_event` as an ordinary
   thread-safe class method.

5. **Host callback protocols.** Do not start until there is an approved solution
   for explicit boxed futures and reverse/nested traits. Map `Tool` plus
   `ToolUpdateSink` first; then `AgentEventSink`; then `SessionStorage` and
   `SessionRepository`; only then auth, policy, catalog, provider, and middleware
   callbacks. The documented target is an exported trait and actual async trait
   methods implemented as Swift protocols.
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
   Acceptance: a Swift tool receives the typed call context, sends a typed update,
   returns a typed output or exact `ToolError`, respects cancellation, and its
   completion acknowledgement orders later agent events; a Swift session backend
   passes the backend-generic storage/recovery conformance suite.

6. **Lossless `AgentEvent` and `AssistantEvent` streams.** Do not accept the
   documented drop-on-full producer for these contracts. BoltFFI's documented
   generated async stream consumes an `Arc<EventSubscription<T>>` and drops new
   events for a full subscriber buffer.
   [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
   [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
   Acceptance: Swift `for await` integration preserves exact event order under a
   deliberately slow consumer, delivers more events than configured capacity
   with zero loss, preserves terminal values, propagates consumer cancellation,
   and passes the repository's replay, agent-event ordering, and sink-barrier
   conformance tests. If any event drops, this phase fails.

7. **Native assembly and core R1 flow.** Map `Models` construction through its
   production provider registration, its `ModelRuntime` implementation into
   `Agent::new`, `ToolRegistry`, the Tokio handle, run start/control/outcome,
   snapshots, retry/continue, deferred fetch/cancel, the separately approved
   deferred-to-agent resumption operation, and reset/shutdown. Acceptance:
   a Swift integration test uses only the same typed items a Rust application
   uses, makes one scripted or captured provider-independent two-turn run,
   observes all assistant and agent events, persists/restores a snapshot, resumes
   one deferred handle, and contains no binding-only command/event envelope.

8. **Extended control plane and session surface.** Map provider/model catalog,
   auth/login, OAuth helpers, concrete API-family option graphs, middleware,
   policy, replay reducers, and durable session values/traits. Documented
   generic free-function/struct/trait blockers need an owner-approved concrete
   strategy; generic inherent methods/constructors, generic enums, and generic
   aliases remain documentation gaps. [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]
   Acceptance: every `core` and `extended` inventory row has a generated symbol
   test or a named, owner-approved deliberate gap; auth challenge/redirect,
   catalog refresh, model options, session append/recovery, and callback ordering
   have Swift integration tests.

9. **Apple packaging and migration decision.** Generate source during iteration,
   then use Apple packaging to create the XCFramework/SwiftPM artifact; BoltFFI
   documents source-only generation and Apple packaging as separate operations.
   [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
   Acceptance: a clean sample Swift package consumes the produced artifact on
   every configured Apple slice, the BoltFFI and UniFFI module names coexist in
   one application, ABI symbols do not collide, and replacement of `pi-ffi` is
   considered only after all earlier acceptance tests and the four repository
   commitment gates pass.
