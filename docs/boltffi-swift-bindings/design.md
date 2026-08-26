# BoltFFI-generated Swift bindings design

Status: revised design, round 12, 2026-08-26. Authority:
`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:1`. That adopted review
retires the earlier “attributes only” interpretation of R2. This document uses
the current crate names. The planned `pi-*` to `agentprism-*` rename changes
package and module names only; the boundary and invariants below do not change.

## 1. Decision and scope

The binding boundary is the concrete Tokio actor facade, not the borrowed
`pi-agent-core::Agent` composition seams. The current actor already owns the
agent on one task, serializes commands through a bounded mailbox, publishes
owned snapshots, and returns one actor-owned run
(`crates/pi-agent-runtime-tokio/src/lib.rs:148`). The low-level `Agent::run`,
`Agent::prompt_text`, `Agent::prompt_records`, `Agent::continue_run`, and
`Agent::retry_last_turn` methods instead borrow `&mut Agent` and return borrowed
`SendBoxStream` values (`crates/pi-agent-core/src/run.rs:283`,
`crates/pi-agent-core/src/run.rs:292`,
`crates/pi-agent-core/src/run.rs:302`,
`crates/pi-agent-core/src/run.rs:312`,
`crates/pi-agent-core/src/run.rs:336`).

`docs/boltffi-swift-bindings/api-inventory.md` remains useful documentation
research, but its broad `core`/`extended` accounting is not the initial binding
scope after the adopted owner review. The verbatim lists below are the scope.

Corrected R2 is:

> Integration must not introduce a separately maintained FFI facade, duplicate
> record hierarchy, IDL, or required Swift wrapper. Existing canonical crates
> may receive inline BoltFFI annotations and minimal concrete API changes needed
> to project their ordinary consumer contracts safely, including owned returns,
> concrete collection inputs, interior synchronization, async pull methods, and
> Rust-owned runtime integration. Every such change must remain a legitimate
> Rust API rather than a binding-only command/envelope layer.

The implementation may therefore improve the canonical Rust actor API where the
same change is useful to an ordinary Rust caller. It may not reproduce the
JSON-configuration construction and sequenced-JSON-event facade currently
maintained in `bindings/pi-ffi`: its JSON constructor and agent-configuration
entry point are at `bindings/pi-ffi/src/lib.rs:190` and
`bindings/pi-ffi/src/lib.rs:198`, while its sequenced JSON event surface begins
at `bindings/pi-ffi/src/lib.rs:290`.

The adopted ordinary-consumer export scope is reproduced verbatim:

- `TokioAgentHandle`
- Reshaped `TokioAgentRun`
- Prompt/continue/retry
- Steering/follow-up/cancellation
- Reset/snapshot/wait-for-idle/shutdown
- Owned Agent event/outcome/control types
- `AgentEventSink` when acknowledged host callbacks are implemented

The adopted initial exclusions are also reproduced verbatim:

- Bare borrowed `Agent` run streams
- `ModelRuntime` trait-object construction seams
- Scheduler streams
- Generic `TypedTool<I, F>`
- Provider implementation traits
- Local/non-Send executor family
- Raw Tokio receivers

Those exclusions correspond to the five borrowed Agent methods cited above,
the runtime trait (`crates/pi-ai/src/runtime.rs:87`), scheduler streams
(`crates/pi-agent-core/src/scheduler.rs:238`), `TypedTool`
(`crates/pi-agent-core/src/tools.rs:277`), the Local Agent family
(`crates/pi-agent-core/src/restore.rs:221`), and the current raw run receiver
(`crates/pi-agent-runtime-tokio/src/lib.rs:133`). Swift-authored tools,
policies, storage backends, model runtimes, and provider implementations are
later callback-authoring milestones, not prerequisites for the first concrete
Agent consumer boundary.

### Design verdict

The initial binding is feasible only after the canonical changes in section 4.
Authoritative Agent delivery uses `AgentEventEnvelope` through
`TokioAgentRun::next_event`; direct model-call `AssistantEvent` delivery uses the concrete
`TokioAssistantStream::next_event` specified in section 4.8. Neither uses
BoltFFI `EventSubscription`.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
`AgentEventSink` remains a distinct acknowledged callback contract. The first
implementation milestone changes canonical Rust APIs; the second adds inline
annotations; the third runs native Swift acceptance tests.

Six highlighted gaps remain implementation gates rather than reasons to
invent a facade:

1. The snapshot does not document consuming class receivers such as
   `outcome(self)`. The owner review reports a rejection in BoltFFI source, but
   that claim is **UNRESOLVED: not answered by the documentation**. Pages
   checked: `classes.md#methods`, `classes.md#memory-management`, and
   `async.md#methods`. The canonical `outcome(&self)` reshape is independently
   required for reusable completion and cancellation safety.
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management]
   [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods]
2. The snapshot documents async host traits and class arguments separately, but
   does not show an async host-trait method receiving an owned Rust-backed class
   such as `CancellationToken`.
   **UNRESOLVED: not answered by the documentation**. Pages checked:
   `callbacks.md#traits`,
   `callbacks.md#async-methods`, `callbacks.md#ownership`, and
   `classes.md#methods-that-take-or-return-classes`.
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
3. The streaming page documents callback-mode delivery through the same
   buffered `EventSubscription`, but it says nothing about a producer-side
   acknowledgement barrier. **UNRESOLVED: not answered by the documentation**.
   Pages checked: `streaming.md#callback-mode`,
   `streaming.md#buffer-capacity`, and `streaming.md#how-it-works`. This is why
   callback-mode streaming is not used as an `AgentEventSink` substitute.
   [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#callback-mode]
   [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
   [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#how-it-works]
4. Current `TokioAgentError::Agent(AgentError)` is both a tuple-payload error
   variant and a nested error-valued payload
   (`crates/pi-agent-runtime-tokio/src/lib.rs:76`). The error documentation
   demonstrates unit variants and struct-style variants whose fields are
   strings or primitives, but it does not establish either tuple-payload error
   variants or nested error-valued payloads.
   **UNRESOLVED: not answered by the documentation** for both questions. Pages
   checked: `errors.md#supported-error-types`, `errors.md#enum-errors`, and
   `errors.md#enums-with-payloads`. Section 8.3 makes this an annotation gate.
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
5. The records documentation demonstrates unit data-enum variants and
   struct-style associated-data variants, but it does not establish whether
   `#[data]` accepts tuple-style variants. This affects at least `Message`,
   `AgentRecord`, `DiagnosticErrorCode`, `ConstrainedSampling`,
   `OrderedJsonValue`, `ReplayTarget`, `OpaquePayload`, and
   `ReplayDataOperation` in the in-scope graph
   (`crates/pi-ai/src/messages.rs:32`,
   `crates/pi-agent-core/src/state.rs:62`,
   `crates/pi-ai/src/messages.rs:178`,
   `crates/pi-ai/src/messages.rs:334`,
   `crates/pi-ai/src/json_compat.rs:324`,
   `crates/pi-ai/src/replay.rs:179`,
   `crates/pi-ai/src/replay.rs:276`,
   `crates/pi-ai/src/streaming.rs:563`).
   **UNRESOLVED: not answered by the documentation**. Pages checked:
   `records.md#enums`, `records.md#enums-with-associated-data`, and
   `types.md#records`. Section 8.2 makes each canonical enum a distinct
   generation and Swift-fidelity gate; no binding-only enum or envelope may
   replace one.
   [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
   [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
6. `ControlError::QueueFull.capacity` is `usize`
   (`crates/pi-agent-core/src/control.rs:68`,
   `crates/pi-agent-core/src/control.rs:74`), and both
   `DEFAULT_COMMAND_CAPACITY` and `DEFAULT_EVENT_CAPACITY` are `usize`
   (`crates/pi-agent-runtime-tokio/src/lib.rs:30`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:33`). The numeric quick-reference
   table lists the fixed-width integer types through `u64`, but not `usize`.
   The primitives section has an isolated `usize` function-argument example;
   it does not state whether `usize` is supported as an error-variant payload
   or a constant type. **UNRESOLVED: not answered by the documentation**.
   Pages checked: `types.md#quick-reference`, `types.md#primitives`,
   `errors.md#enums-with-payloads`, and `constants.md#supported-values`.
   `ControlError::QueueFull.capacity` is therefore a separate generated Swift
   payload-fidelity gate. The two capacity constants remain unannotated in the
   initial binding; tests may use them inside Rust, but no generated constant
   is assumed.
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
   [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]

### Owner-review coverage

| Adopted review section | Design resolution |
|---|---|
| Corrected R2 (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:12`) | Section 1 and all of section 4 |
| Core streaming fact (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:33`) | Sections 3.2–3.4 |
| Tokio actor rather than bare `Agent` (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:63`) | Section 1 and section 5 |
| Current delivery path (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:84`) | Section 3.1 |
| `TokioAgentRun` reshape and raw receiver (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:130`) | Sections 4.1–4.2 |
| Cancellation semantics (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:236`) | Section 6 |
| Acknowledged sinks (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:272`) | Section 4.4 |
| Sink-only run fix (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:324`) | Section 4.3 |
| Four Swift consumer shapes (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:339`) | Section 7 |
| Event envelope decision (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:395`) | Section 4.5 |
| Tokio runtime ownership (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:418`) | Section 4.7 |
| Ordinary-consumer scope (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:435`) | Sections 1 and 5 |
| Twelve acceptance tests (`docs/boltffi-swift-bindings/owner-review-2026-08-26.md:461`) | Section 9 |

The round-5 rejection is resolved explicitly as follows: section 4.7 now gives
Swift a concrete OpenAI provider/native-transport/API-key factory that returns a
configured canonical `Models`, rather than assuming that empty `Models::new()`
is production construction; sections 5 and 8.7 carry that path through the
surface and gap analysis; acceptance test 17 executes it through generated
Swift. Section 8.4 gates all six currently in-scope `#[non_exhaustive]`
occurrences, including `AssistantEvent` and the `RequestStartErrorKind` inside
`RequestStartError`. Section 8.2 now records `BTreeSet`,
`serde_json::Number`, and `serde_json::Value` as unresolved transitive value
types. This revision also traces the previously omitted
`ToolCall.arguments`, `ToolSpec.parameters`, `DeferredHandle.data`,
`ToolOutput.details`, `ToolUpdate.details`, `ToolResultMessage.details`/
`VersionedExtension.value`, and `GrammarVariants` paths. Sections 5 and 10
carry those paths into their surface-level and generation gates; the document
does not infer coverage for one JSON or map field from a probe of another.

The round-7 rejection is resolved explicitly as follows. Sections 5, 8.2, 9,
and 10 now treat `AgentSnapshot.streaming` as an independent active/partial
snapshot root rather than inferring it from `AgentSnapshot.state` or a
committed transcript (`crates/pi-agent-core/src/state.rs:184`,
`crates/pi-agent-core/src/state.rs:188`). Acceptance test 18 now constructs
standalone `AssistantEvent::DiagnosticAdded`, each of the three terminal
`AssistantEvent` variants, nested `AgentEvent::AssistantUpdate`, explicit
`AgentEvent::MessageCommitted`, and a snapshot whose `streaming` field is
`Some(AssistantMessageSnapshot)` with values deliberately different from the
committed transcript (`crates/pi-ai/src/streaming.rs:428`,
`crates/pi-ai/src/streaming.rs:522`, `crates/pi-ai/src/streaming.rs:528`,
`crates/pi-ai/src/streaming.rs:534`,
`crates/pi-agent-core/src/events.rs:113`,
`crates/pi-agent-core/src/events.rs:120`,
`crates/pi-ai/src/streaming.rs:1674`). The tuple-newtype gate now covers not
only IDs but also `Timestamp`, `Currency`, `ReplayDropReason`, and the
`OrderedJsonObject`/`OrderedJsonString`/`OrderedJsonArray` sampling graph
(`crates/pi-ai/src/ids.rs:133`, `crates/pi-ai/src/usage.rs:88`,
`crates/pi-ai/src/handoff.rs:44`, `crates/pi-ai/src/json_compat.rs:24`,
`crates/pi-ai/src/json_compat.rs:114`,
`crates/pi-ai/src/json_compat.rs:227`). The documentation shows named-field
struct records but does not establish tuple-newtype generation, so each of
these is an explicit unresolved generation and fidelity gate rather than an
inference from an ID probe. **UNRESOLVED: not answered by the documentation**.
Pages checked: `records.md#structs`, `types.md#records`, and
`custom-types.md#representation-types`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]

The round-8 rejection is resolved explicitly as follows. Sections 5, 8.2, 9,
and 10 now traverse the full replay graph rather than treating `replay` as an
opaque leaf: `AssistantMessageSnapshot.replay` is a `ReplayEnvelope` with a
`ReplayScope` and ordered `Vec<ReplayItem>`, and every item reaches
`ReplayTarget` and `OpaquePayload` (`crates/pi-ai/src/streaming.rs:1697`,
`crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:17`,
`crates/pi-ai/src/replay.rs:19`, `crates/pi-ai/src/replay.rs:135`,
`crates/pi-ai/src/replay.rs:141`, `crates/pi-ai/src/replay.rs:149`). The same
matrix is required independently in `AgentSnapshot.streaming.replay`, that
snapshot's `terminal_message.replay`, standalone assistant replay events, and
nested `AgentEvent::AssistantUpdate` replay events. Acceptance test 18 requires
nonempty ordered items, every `ReplayTarget` form, all three
`OpaquePayload::{Utf8, Bytes, JsonBytes}` forms, every
`ReplayDataOperation` form, and distinct Swift-checked sentinels at each root
(`crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`,
`crates/pi-ai/src/streaming.rs:474`, `crates/pi-ai/src/streaming.rs:488`,
`crates/pi-ai/src/streaming.rs:563`). The same sections now make tuple-payload
data-enum syntax an explicit unresolved gate for all eight enums listed in gap
5 rather than assuming that the documented struct-style associated-data syntax
covers it. The records page does not show tuple-variant syntax.
**UNRESOLVED: not answered by the documentation**. Pages checked:
`records.md#enums`, `records.md#enums-with-associated-data`, and
`types.md#records`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]

The round-9 replay-traversal rejection is resolved explicitly as follows.
Sections 5, 8.2, 9, and 10 now name and test thirteen independent
`ReplayEnvelope` roots: a direct `ModelRequest` assistant message; each direct
`AssistantEvent::{Finished,Failed,Cancelled}` terminal message; each of those
three terminal events nested in `AgentEvent::AssistantUpdate`; an assistant
`AgentEvent::MessageCommitted`; a standalone assistant `AgentRecord`; a direct
`AgentState.transcript`; `AgentSnapshot.state.transcript`;
`AgentSnapshot.streaming.replay`; and
`AgentSnapshot.streaming.terminal_message.replay`
(`crates/pi-ai/src/runtime.rs:17`, `crates/pi-ai/src/messages.rs:36`,
`crates/pi-ai/src/messages.rs:157`, `crates/pi-ai/src/messages.rs:473`,
`crates/pi-ai/src/streaming.rs:521`, `crates/pi-ai/src/streaming.rs:528`,
`crates/pi-ai/src/streaming.rs:534`,
`crates/pi-agent-core/src/events.rs:113`,
`crates/pi-agent-core/src/events.rs:120`,
`crates/pi-agent-core/src/state.rs:33`,
`crates/pi-agent-core/src/state.rs:64`,
`crates/pi-agent-core/src/state.rs:184`,
`crates/pi-agent-core/src/state.rs:188`,
`crates/pi-ai/src/streaming.rs:1697`,
`crates/pi-ai/src/streaming.rs:1705`). Every root has a distinct, nonempty
envelope with root-specific scope and item sentinels, ordered items, all four
`ReplayTarget` variants, and all three `OpaquePayload` variants. Standalone and
nested `ReplayItemStarted`/`ReplayData` event matrices remain additional roots;
they cannot substitute for any terminal, request, committed, state, or snapshot
envelope.

The remaining accepted-run establishment rejection is resolved explicitly as
follows. Section 4.1 now requires an armed `request_run` drop guard from command
submission through the non-awaiting `TokioAgentRun` return step. If the
exported prompt/continue/retry future is dropped after actor acceptance but
before that handoff, the guard cancels the shared run token and closes the
unclaimed observation receiver. Section 6 distinguishes that establishment
cancellation from cancelling an established `nextEvent()` await or explicitly
cancelling a run. Acceptance test 19 deterministically holds the establishment
future after actor acceptance and before result handoff, then proves terminal
cancellation settlement, actor idleness, and release of the actor's runtime
lease after orderly shutdown in both Rust and generated Swift. Phases 1 and 3
make the corresponding Rust and Swift halves blocking gates.

The latest transitive-boundary rejection is resolved explicitly as follows.
Sections 5 and 8.2 now identify the `usize` payload of
`ControlError::QueueFull` independently of its tuple-newtype and
`#[non_exhaustive]` gates (`crates/pi-agent-core/src/control.rs:68`,
`crates/pi-agent-core/src/control.rs:74`). Acceptance test 18 now has a
generated Swift catch and exact-payload-fidelity gate for that `usize` field.
Section 5 and phase 2 also select a definite policy for
`DEFAULT_COMMAND_CAPACITY` and `DEFAULT_EVENT_CAPACITY`: both remain
unannotated because their current type is `usize`
(`crates/pi-agent-runtime-tokio/src/lib.rs:30`,
`crates/pi-agent-runtime-tokio/src/lib.rs:33`), and the documentation does not
establish `usize` error-payload or constant support. **UNRESOLVED: not answered
by the documentation**. Pages checked: `types.md#quick-reference`,
`types.md#primitives`, `errors.md#enums-with-payloads`, and
`constants.md#supported-values`.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
[https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]

## 2. BoltFFI per-page capability summary

The local manifest records nineteen canonical `/docs/` pages captured on
2026-08-25 (`docs/boltffi-swift-bindings/docs-snapshot/MANIFEST.md:1`).
`async.md` and `streaming.md` were re-read in full for this revision.

- **Overview.** BoltFFI lists Swift as a supported target.
  [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#supported-languages]
  It identifies records, functions, classes, constants, async functions,
  callbacks/traits, async streams, and errors as export categories.
  [https://www.boltffi.dev/docs/overview.md | docs/boltffi-swift-bindings/docs-snapshot/overview.md#what-you-can-export]

- **Installation.** The documented attachment adds `boltffi` as a normal and
  build dependency, includes `staticlib` in the crate type, calls
  `boltffi::build::generate()` from `build.rs`, and uses `boltffi check`.
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]

- **Quick Start.** The quick start shows Cargo and `build.rs` setup, then uses
  `use boltffi::{data, export};` before applying `#[data]` and `#[export]`; it
  also shows initialization, Swift generation, and Apple packaging.
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#2-configure-cargotoml]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#3-create-buildrs]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#4-write-your-rust-code]
  [https://www.boltffi.dev/docs/quick-start.md | docs/boltffi-swift-bindings/docs-snapshot/quick-start.md#5-build-and-generate-bindings]

- **Getting Started.** The page introduces `#[data]` values, `#[export]` free
  functions, and the build/generate/package choices before importing the
  generated Swift module.
  [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
  [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#build-package-or-generate]

- **Tutorial.** The tutorial combines a data value, Rust-owned class, throwing
  method, and async method and shows the corresponding Swift value, class,
  `throws`, and `async` forms.
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#write-the-rust-code]
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#adding-error-handling]
  [https://www.boltffi.dev/docs/tutorial.md | docs/boltffi-swift-bindings/docs-snapshot/tutorial.md#adding-async]

- **Functions.** `#[export]` covers the documented primitives, strings,
  records, enums, slices, optional inputs, classes, callback traits, `Option`,
  `Result`, `Vec`, async functions, and non-stored closures. Generic free
  functions, references returned by free functions, and stored/outliving
  closures are listed as limitations.
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#functions]
  [https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#limitations]

- **Records.** `#[data]` maps the documented structs, unit-only enums, and
  struct-style associated-data enums to Swift value types. The page does not
  show tuple-style data-enum variants; that syntax is
  **UNRESOLVED: not answered by the documentation** after checking
  `records.md#enums`, `records.md#enums-with-associated-data`, and
  `types.md#records`. `#[data(impl)]` exports constructors and methods, and an
  `&mut self` record method becomes mutating Swift.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#methods-and-constructors]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#mutating-methods]

- **Classes.** `#[export]` on an inherent impl maps a Rust-owned object to a
  Swift reference-semantics class. Documented class methods may be
  constructors, synchronous, async, static, throwing, class-valued, or skipped
  with `#[skip]`.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
  Exported classes must be `Send + Sync` by default. The documented
  `single_threaded` mode disables that check and the mutable-receiver check and
  leaves serialization to the target.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]

- **Constants.** `#[export]` maps documented primitive, string, and byte-slice
  constants to module values or static members.
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#global-constants]
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]
  The supported-values section does not enumerate `usize`; the numeric
  quick-reference table lists fixed-width integers but omits `usize`, and the
  isolated `usize` function-argument example does not answer constant support.
  **UNRESOLVED: not answered by the documentation**. Pages checked:
  `constants.md#supported-values`, `types.md#quick-reference`, and
  `types.md#primitives`.
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]

- **Types.** The type tables map the listed primitives, strings, `Option`,
  `Result`, and `Vec`; built-ins include `Duration`, `SystemTime`, `Uuid`, `Url`,
  and `Vec<u8>`.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types]
  Generic structs, arbitrary `dyn Trait`, raw pointers, non-static lifetimes,
  and `HashSet` are listed as unsupported.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
  The numeric quick-reference table lists fixed-width integers only and does
  not list `usize`. Although the primitives section uses `usize` in one
  function-argument example, it does not state support for `usize` in record or
  error payload fields or in constants. **UNRESOLVED: not answered by the
  documentation**. Pages checked: `types.md#quick-reference`,
  `types.md#primitives`, `errors.md#enums-with-payloads`, and
  `constants.md#supported-values`.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
  [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]

- **Callbacks & Traits.** `#[export]` on a trait generates a host-implemented
  Swift protocol. The documented ownership forms are `Box<dyn Trait>` and
  `Arc<dyn Trait>`; async requirements use `#[async_trait]` and actual
  `async fn`; stored callbacks must be owned; multithreaded callbacks use
  `Send + Sync`.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#storing-traits]
  Generic traits and associated types are unsupported, and default
  implementations are ignored.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations]

- **Async.** An exported Rust `async fn` becomes a Swift async function, and
  async `Result` becomes `async throws`. Swift task cancellation cooperatively
  cancels that Rust future.
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#standalone-functions]
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
  BoltFFI provides future polling but no executor; Tokio-dependent work needs
  an active Tokio runtime.
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]

- **Async Internals.** The documented async ABI has entry, poll, complete,
  cancel, and free operations with continuation callbacks; cancellation marks
  and wakes the future before the target reports cancellation.
  [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#generated-ffi-functions]
  [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]

- **Streaming.** `#[ffi_stream(item = T, mode = \"async\")]` requires the
  annotated method to return `Arc<EventSubscription<T>>` and generates Swift
  `AsyncStream<T>`; callback and batch modes use the same subscription
  abstraction.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stream-modes]
  Each subscription uses a finite ring buffer, with 256 as the documented
  default; when full, new events are dropped and the producer does not block.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
  Producer unsubscribe completes iteration; cancelling the Swift task or
  breaking iteration cancels the subscription.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stopping-streams]
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#consumer-side-cancellation]

- **Errors.** Exported `Result<T, E>` becomes a throwing target call when `E`
  is a documented string, `#[error]` struct, or `#[error]` enum; the same rule
  applies to async results.
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]

- **Custom Types.** `custom_type!` converts an external or otherwise non-native
  type to a supported representation; `#[custom_ffi]` plus
  `CustomFfiConvertible` provides manual owned conversion. Documented
  representations include primitives, `String`, `Vec`, and BoltFFI data types.
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-custom_type-macro]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#the-customfficonvertible-trait]
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]
  Failed custom conversion panics rather than producing a recoverable boundary
  error.
  [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors]

- **Configuration.** Root `boltffi.toml` configuration selects package
  identity, source crate, Apple settings, Swift module name, SwiftPM layout,
  slices, symbols, and documented type-mapping overrides.
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#apple-configuration]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#type-mappings]

- **Packaging.** `boltffi generate swift` generates source, while
  `boltffi pack apple` builds Rust, generates Swift, and produces an
  XCFramework and Swift package with documented bundled, split, or FFI-only
  layouts.
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#overview]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#swiftpm-layouts]

- **Experimental Features.** Experimental mode is configured by CLI or
  configuration; the listed experimental stream work concerns Kotlin
  Multiplatform and TypeScript rather than Swift.
  [https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#enabling]
  [https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#feature-details]

## 3. Authoritative streaming rule

### 3.1 Current delivery path

The current concrete path is:

```text
borrowed Agent stream
        |
        | actor polls
        v
bounded Tokio mpsc
        |
        | sender.send(event).await
        v
TokioAgentRun::next_event
        |
        v
consumer
```

The actor creates a run-local `CancellationToken`, polls the borrowed core
stream, applies each event to the published snapshot, and then dispatches it
(`crates/pi-agent-runtime-tokio/src/lib.rs:511`,
`crates/pi-agent-runtime-tokio/src/lib.rs:734`). The observational channel is
bounded at a default capacity of 128
(`crates/pi-agent-runtime-tokio/src/lib.rs:33`). `dispatch_event` awaits
`sender.send(event.clone())`; a full channel therefore delays actor progress
rather than discarding that event
(`crates/pi-agent-runtime-tokio/src/lib.rs:838`).

Current ordering for each event is:

1. Apply the event and publish the snapshot
   (`crates/pi-agent-runtime-tokio/src/lib.rs:754`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:761`).
2. Send the observation to the run channel
   (`crates/pi-agent-runtime-tokio/src/lib.rs:838`).
3. Await registered sinks in registration order
   (`crates/pi-agent-runtime-tokio/src/lib.rs:842`).
4. Await the run-scoped sink
   (`crates/pi-agent-runtime-tokio/src/lib.rs:848`).
5. Return to polling the core stream
   (`crates/pi-agent-runtime-tokio/src/lib.rs:748`).

`RunFinished` is captured before dispatch, but the completion result and idle
notification are sent only after `drive_run` returns, so they include
`RunFinished` sink settlement
(`crates/pi-agent-runtime-tokio/src/lib.rs:762`,
`crates/pi-agent-runtime-tokio/src/lib.rs:718`).

### 3.2 Why `EventSubscription` is forbidden here

Every documented `#[ffi_stream]` mode requires
`Arc<EventSubscription<T>>`; the Swift async mode is the one that generates
`AsyncStream<T>`.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#async-mode]
The same documented subscription has a finite ring buffer and drops new events
when full without blocking the producer.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]

Therefore:

- No exported method returning `EventSubscription<AgentEventEnvelope>` or
  `EventSubscription<AgentEvent>` is permitted. The selected canonical Tokio
  item is `AgentEventEnvelope`, whose payload is `AgentEvent`
  (`crates/pi-agent-core/src/events.rs:329`,
  `crates/pi-agent-core/src/events.rs:335`).
- No exported method returning `EventSubscription<AssistantEvent>` is
  permitted. `AssistantEvent` is the lossless normalized stream nested in
  `AgentEvent::AssistantUpdate`
  (`crates/pi-ai/src/streaming.rs:360`,
  `crates/pi-agent-core/src/events.rs:113`,
  `crates/pi-agent-core/src/events.rs:117`).
- Increasing the ring capacity is not a semantic fix because the documented
  overflow rule still drops.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
- Callback and batch stream modes are also unsuitable because they use
  `EventSubscription<T>` as well.
  [https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stream-modes]

Adding a BoltFFI ring after the existing Tokio channel would introduce a new
loss point only at the foreign-language boundary.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]

### 3.3 Exported Agent pull contract

The exported boundary is an ordinary async class method:

```rust
pub async fn next_event(
    &self,
) -> Result<Option<AgentEventEnvelope>, TokioAgentError>;
```

Section 4.5 selects `AgentEventEnvelope` as the canonical Tokio observation and
sink item. Exported Rust async class methods map to Swift async methods,
`Result` maps to throwing calls, and `Option<T>` maps to a Swift optional.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option]
Composing those documented mappings, the expected Swift consumption shape is
an inference:

```swift
while let envelope = try await run.nextEvent() {
    consume(envelope)
}
```

This method pulls directly from the Rust-owned Tokio receiver. It adds no second
queue and retains the actor's current backpressure behavior
(`crates/pi-agent-runtime-tokio/src/lib.rs:127`,
`crates/pi-agent-runtime-tokio/src/lib.rs:399`,
`crates/pi-agent-runtime-tokio/src/lib.rs:838`).

### 3.4 Exported direct model-call pull contract

R3 also covers model calls made directly through the concrete `Models` control
plane, not only assistant events nested in an Agent run. Today
`Models::stream_simple` takes an owned `ModelRequest` and `CancellationToken`
and returns a future establishing an owned `AssistantStream`
(`crates/pi-ai/src/models.rs:762`). `ModelRuntime::stream` exposes the same
request/stream contract at the narrow execution seam
(`crates/pi-ai/src/runtime.rs:89`). `AssistantStream` is a
`Stream<Item = AssistantEvent>` fused after a terminal event or raw EOF
(`crates/pi-ai/src/streaming.rs:1900`,
`crates/pi-ai/src/streaming.rs:1934`). The trait-object seam remains
unannotated; the concrete `Models` path is projected by the canonical Tokio
owner in section 4.8.

The exported item is a Rust-owned class with this lossless pull:

```rust
pub async fn next_event(
    &self,
) -> Result<Option<AssistantEvent>, TokioAssistantError>;
```

An exported async class method maps to a Swift async method, `Result` maps to a
throwing call, and `Option<T>` maps to a Swift optional.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option]
Composing those documented mappings, the expected Swift loop is an inference:

```swift
while let event = try await stream.nextEvent() {
    consume(event)
}
```

The direct stream uses a bounded Tokio channel whose sender awaits capacity; it
does not use `EventSubscription<AssistantEvent>`. The documented BoltFFI
subscription drops new events on overflow instead of applying backpressure, so
it cannot carry this authoritative stream.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]

Request establishment failure remains a thrown `RequestStartError`. After an
`AssistantStream` exists, successful, failed, and cancelled completion are the
existing terminal `AssistantEvent::{Finished,Failed,Cancelled}` values
(`crates/pi-ai/src/runtime.rs:42`,
`crates/pi-ai/src/streaming.rs:521`,
`crates/pi-ai/src/streaming.rs:528`,
`crates/pi-ai/src/streaming.rs:534`). `next_event` returns `Ok(None)` only after
one of those terminal events has been delivered. If the producer reaches raw
EOF first, the next pull throws `TokioAssistantError::MissingTerminalEvent`;
the current raw `AssistantStream` otherwise fuses on that EOF without
distinguishing it (`crates/pi-ai/src/streaming.rs:1948`). Actor/task failure is
reported as `TokioAssistantError::Closed`. These errors are canonical runtime
protocol errors, not replacement provider envelopes.

## 4. Canonical Rust API changes

All changes in this section belong in existing canonical crates. None creates a
binding crate, duplicate record hierarchy, IDL, command dispatcher, or required
Swift wrapper.

### 4.1 Reshape `TokioAgentRun`

The current `TokioAgentRun` directly owns
`mpsc::Receiver<AgentEvent>` and a consuming
`oneshot::Receiver<Result<RunOutcome, TokioAgentError>>`
(`crates/pi-agent-runtime-tokio/src/lib.rs:127`,
`crates/pi-agent-runtime-tokio/src/lib.rs:128`). Its methods are currently
`next_event(&mut self) -> Option<AgentEvent>` and
`outcome(self) -> Result<RunOutcome, TokioAgentError>`
(`crates/pi-agent-runtime-tokio/src/lib.rs:138`,
`crates/pi-agent-runtime-tokio/src/lib.rs:143`). Calling current `outcome()`
without draining more than the 128-event channel capacity can wait forever:
the actor can be blocked in `send().await` while `outcome` waits on the
completion oneshot (`crates/pi-agent-runtime-tokio/src/lib.rs:399`,
`crates/pi-agent-runtime-tokio/src/lib.rs:838`).

Use interior synchronization and reusable completion:

```rust
pub struct TokioAgentRun {
    events: tokio::sync::Mutex<mpsc::Receiver<AgentEventEnvelope>>,
    completion:
        watch::Receiver<Option<Result<RunOutcome, TokioAgentError>>>,
    cancellation: CancellationToken,
    // Private state records observation-closed and terminal validation.
}
```

This is the section 4.5 decision applied to the run storage.
`TokioAgentError` should derive `Clone` so a cached completion can be returned
more than once; all of its current payloads are cloneable
(`crates/pi-agent-runtime-tokio/src/lib.rs:70`,
`crates/pi-agent-core/src/error.rs:13`).

The one annotated inherent impl has this canonical API:

```rust
#[export]
impl TokioAgentRun {
    pub async fn next_event(
        &self,
    ) -> Result<Option<AgentEventEnvelope>, TokioAgentError>;

    pub async fn outcome(
        &self,
    ) -> Result<RunOutcome, TokioAgentError>;

    pub fn cancel(&self);

    pub async fn cancel_and_outcome(
        &self,
    ) -> Result<RunOutcome, TokioAgentError>;
}
```

These are the selected canonical Rust method shapes. Applying `#[export]` to
the impl remains gated on the two `TokioAgentError::Agent(AgentError)` mapping
questions in section 8.3; the design does not infer support for that error shape
from the documented async-method mapping.
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

`#[export]` on an inherent impl is the documented Rust-class mapping; async
`&self` methods are documented, and the default exported-class check requires
`Send + Sync`.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
The implementation must pass that default check; it must not use
`#[export(single_threaded)]`.

`request_run` must create the `CancellationToken` before it submits
`RunChannels`, pass one clone to the actor, and retain one clone in the returned
`TokioAgentRun`. The actor currently creates separate tokens inside each
prompt/continue/retry branch
(`crates/pi-agent-runtime-tokio/src/lib.rs:511`,
`crates/pi-agent-runtime-tokio/src/lib.rs:546`,
`crates/pi-agent-runtime-tokio/src/lib.rs:578`,
`crates/pi-agent-runtime-tokio/src/lib.rs:616`). Moving creation to
`request_run` makes run-local cancellation available immediately without
adding a mailbox command or a second cancellation identity.

`request_run` is also a cancellation-sensitive establishment boundary. Its
current implementation creates the event/completion/acceptance channels,
submits the actor command, awaits `accepted_rx`, and only then constructs the
returned `TokioAgentRun`
(`crates/pi-agent-runtime-tokio/src/lib.rs:394`,
`crates/pi-agent-runtime-tokio/src/lib.rs:399`,
`crates/pi-agent-runtime-tokio/src/lib.rs:411`,
`crates/pi-agent-runtime-tokio/src/lib.rs:412`). On the actor side,
`accept_run` marks the actor non-idle and sends acceptance
(`crates/pi-agent-runtime-tokio/src/lib.rs:697`,
`crates/pi-agent-runtime-tokio/src/lib.rs:704`,
`crates/pi-agent-runtime-tokio/src/lib.rs:705`); the prompt branch can then
enter `drive_run` immediately (`crates/pi-agent-runtime-tokio/src/lib.rs:523`).
The continue and retry branches have the same transition
(`crates/pi-agent-runtime-tokio/src/lib.rs:596`,
`crates/pi-agent-runtime-tokio/src/lib.rs:634`). Thus acceptance can wake the
establishment future while the actor has already begun an accepted run.

Add a private canonical `RunEstablishmentGuard` inside `request_run`. It is
armed before command submission and owns the not-yet-handed-off event receiver,
the reusable completion receiver, and one clone of the same cancellation token
placed in `RunChannels`. Conceptually:

```rust
struct RunEstablishmentGuard {
    events: Option<mpsc::Receiver<AgentEventEnvelope>>,
    completion:
        Option<watch::Receiver<Option<Result<RunOutcome, TokioAgentError>>>>,
    cancellation: Option<CancellationToken>,
    armed: bool,
}
```

While armed, `Drop` must cancel the token and close/drop the event receiver,
discarding any buffered observation. It also drops the unclaimed completion
receiver. After successful actor acceptance, one non-awaiting `handoff` method
moves those three values into `TokioAgentRun` and disarms the guard as the final
step that produces `Ok(TokioAgentRun)`. There must be no suspension point
between disarming and returning the run. Rejection and every early return leave
the guard armed. Merely sending or receiving actor acceptance never disarms it.
This is private RAII inside the canonical actor API, not a binding-only object.

This guard closes the specific post-acceptance race: if Swift cancels an
exported prompt/continue/retry establishment await after the actor sent
acceptance but before the generated binding repolls and receives the run,
BoltFFI cooperatively stops further polling of that Rust future; its documented
cleanup path then reaches `free`.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
[https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]
Freeing the still-pending future drops the armed guard, which is the proposed
Rust cleanup action that cancels the actor's shared token and closes
observation. Without that explicit cancellation, closing the current receiver
only makes `dispatch_event` disable subsequent observational sends; it does not
cancel the actor-owned token or core run
(`crates/pi-agent-runtime-tokio/src/lib.rs:837`,
`crates/pi-agent-runtime-tokio/src/lib.rs:840`). The actor must continue far
enough to settle the cancellation lifecycle and sinks and publish idle even
though no `TokioAgentRun` was handed to the caller; an ensuing orderly shutdown
must then let `actor_loop` exit and release its runtime lease. Section 9 test 19
is the mandatory race proof.

`next_event` must:

1. Hold the receiver mutex across one `recv` so concurrent polls serialize.
2. Return the next owned envelope unchanged.
3. Track whether its embedded event delivered a matching `RunFinished`.
4. On channel EOF, await/read cached actor completion.
5. Return `Ok(None)` only after a validated normal end.
6. Return the existing `MissingRunFinished`, `SnapshotInvariant`, or `Closed`
   variant for protocol/actor failure as applicable
   (`crates/pi-agent-runtime-tokio/src/lib.rs:74`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:78`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:80`).

Expected provider failure and cancellation remain in-band
`RunOutcome::Failed` and `RunOutcome::Cancelled` values
(`crates/pi-agent-core/src/events.rs:62`,
`crates/pi-agent-core/src/events.rs:69`). They do not become thrown transport
errors.

`outcome(&self)` preserves the practical meaning of the old consuming method:
the caller no longer intends to observe intermediate events. It must acquire
the receiver, close it, discard all buffered observations, thereby wake any
actor send blocked on capacity, and then await the reusable cached completion.
After `outcome` begins, later `next_event` calls return `Ok(None)` after a
successful cached completion or return the same cached actor/protocol error.
Cancelling one `outcome` await must not consume or destroy the completion for a
later call.

`cancel(&self)` cancels the retained run token without waiting behind the actor
mailbox. `cancel_and_outcome(&self)` cancels that token, closes/discards
observations using the same operation as `outcome`, and waits for terminal
settlement.

Keep advanced receiver access in a separate unannotated Rust-only impl:

```rust
impl TokioAgentRun {
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEventEnvelope> {
        self.events.get_mut()
    }
}
```

Raw Tokio envelope receivers are not part of the generated boundary.
BoltFFI's documented `#[skip]` can also exclude a class method, but physically
separating the impl keeps the foreign-facing block concrete and auditable.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]

### 4.2 Validate EOF at the pull boundary

Current `drive_run` already converts core EOF without `RunFinished` to
`TokioAgentError::MissingRunFinished`
(`crates/pi-agent-runtime-tokio/src/lib.rs:823`,
`crates/pi-agent-runtime-tokio/src/lib.rs:824`). The current event receiver,
however, reports only `None`; a caller can miss the actor error unless it also
consumes `outcome` (`crates/pi-agent-runtime-tokio/src/lib.rs:138`).

The reshaped pull method couples EOF to cached completion. Normal EOF requires:

- a delivered `RunFinished`;
- a matching cached `RunOutcome`;
- no snapshot-assembly error; and
- settled run-scoped and registered sinks.

Snapshot assembly errors already use `SnapshotInvariant`
(`crates/pi-agent-runtime-tokio/src/lib.rs:859`). Sender/owner-task loss uses
`Closed` (`crates/pi-agent-runtime-tokio/src/lib.rs:74`). This distinction is
the Rust API contract as well as the Swift throwing contract.

### 4.3 Fix sink-only runs

`prompt_text_with_sink` currently calls the same `request_run` path as a pull
run (`crates/pi-agent-runtime-tokio/src/lib.rs:217`).
`request_run` always creates the bounded event channel
(`crates/pi-agent-runtime-tokio/src/lib.rs:399`), and `dispatch_event` awaits
that sender before it invokes the run-scoped sink
(`crates/pi-agent-runtime-tokio/src/lib.rs:838`,
`crates/pi-agent-runtime-tokio/src/lib.rs:848`). A caller that ignores the run
receiver can therefore stop sink delivery after the channel fills.

Make the observation sender optional in canonical `RunChannels` and
`drive_run`, with item type
`Option<mpsc::Sender<AgentEventEnvelope>>`. A pull run installs
`Some(sender)`; a sink-only run installs `None`. `prompt_text_with_sink` still
returns `TokioAgentRun` so callers can cancel and await outcome, but the run is
constructed in observation-closed mode and no hidden drainer is spawned. This
is preferable to requiring callers to race an immediate `outcome()` call.

The existing UniFFI binding's background drain at
`bindings/pi-ffi/src/lib.rs:690` is not copied.

### 4.4 Export `AgentEventSink` as an acknowledged async trait

The current trait returns an explicit
`SendBoxFuture<'static, ()>` (`crates/pi-agent-runtime-tokio/src/lib.rs:45`).
Rewrite that same canonical trait:

```rust
#[async_trait::async_trait]
#[export]
pub trait AgentEventSink: Send + Sync + 'static {
    async fn on_event(
        &self,
        envelope: AgentEventEnvelope,
        cancellation: CancellationToken,
    );
}
```

BoltFFI documents `#[export]` host traits, `Arc<dyn Trait>` shared/stored
ownership, `Send + Sync` for multithreaded callbacks, and
`#[async_trait]` plus actual `async fn`; when Rust calls the async method it
awaits the target implementation before continuing.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#thread-safety]

The repository-side acknowledgement contract remains explicit:
`dispatch_event` awaits each registered sink in order and then awaits the
run-scoped sink before it returns to `drive_run`
(`crates/pi-agent-runtime-tokio/src/lib.rs:765`,
`crates/pi-agent-runtime-tokio/src/lib.rs:842`,
`crates/pi-agent-runtime-tokio/src/lib.rs:848`). The async-trait page says a
Rust caller awaits the target implementation before continuing.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]

By contrast, whether BoltFFI's callback-mode *stream* provides any
producer-side acknowledgement barrier is
**UNRESOLVED: not answered by the documentation**. Pages checked:
`streaming.md#callback-mode`,
`streaming.md#buffer-capacity`, and `streaming.md#how-it-works`. Those pages
describe a callback handle, a finite ring buffer, event dropping, and batched
consumer polling, but no acknowledgement from target callback completion to the
Rust producer. Therefore callback-mode `EventSubscription` is not a permitted
sink replacement.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#callback-mode]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#how-it-works]

The entire annotated signature above is conditional on resolving the owned
`CancellationToken` callback-argument gap identified in section 1. The
documentation separately shows async trait methods and methods that pass
Rust-backed classes, but does not show their combination.
**UNRESOLVED: not answered by the documentation**; pages checked:
`callbacks.md#async-methods`, `callbacks.md#ownership`, and
`classes.md#methods-that-take-or-return-classes`. The annotation milestone must
generate and execute a minimal Swift callback before the sink surface is
accepted. If it fails, acknowledged Swift sinks remain blocked and the design
must return to the owner; it may not replace the token with a binding-only
integer or command.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]

Preserve native closure ergonomics with a blanket implementation that changes
the current item from `AgentEvent` to `AgentEventEnvelope`, accepts
`Fn(AgentEventEnvelope, CancellationToken) -> SendBoxFuture<'static, ()>`, and
awaits that future inside the new async trait method. The current bare-event
blanket implementation is at `crates/pi-agent-runtime-tokio/src/lib.rs:54`;
this is part of the canonical runtime item promotion, not a binding-only
adapter.

Inside a sink, only re-entrant capabilities are safe:
`CancellationToken::cancel` (`crates/pi-ai/src/cancellation.rs:62`),
`TokioAgentHandle::cancel_now`
(`crates/pi-agent-runtime-tokio/src/lib.rs:301`), and
`TokioAgentHandle::latest_snapshot`
(`crates/pi-agent-runtime-tokio/src/lib.rs:364`). Awaiting a mailbox method from
the sink can wait behind that sink's own acknowledgement because the actor is
inside `dispatch_event` until the sink returns
(`crates/pi-agent-runtime-tokio/src/lib.rs:765`).

### 4.5 Decision: promote the canonical event item to `AgentEventEnvelope`

The code defines the persistence/FFI value:

```rust
pub struct AgentEventEnvelope {
    pub sequence: u64,
    pub run_id: RunId,
    pub event: AgentEvent,
}
```

(`crates/pi-agent-core/src/events.rs:329`,
`crates/pi-agent-core/src/events.rs:331`,
`crates/pi-agent-core/src/events.rs:333`,
`crates/pi-agent-core/src/events.rs:335`). Architecture Part 1 says to wrap
events in this form for persistence and FFI
(`docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part1-proposal.md:1018`).
The current Tokio run, channel, and sink use bare `AgentEvent`
(`crates/pi-agent-runtime-tokio/src/lib.rs:127`,
`crates/pi-agent-runtime-tokio/src/lib.rs:420`,
`crates/pi-agent-runtime-tokio/src/lib.rs:49`).

The two legitimate options from the owner review were:

1. Preserve the current ordinary Rust actor API. `next_event` and
   `AgentEventSink::on_event` continue carrying bare `AgentEvent`.
2. Promote `AgentEventEnvelope` to the canonical Tokio observation and sink
   item. The actor allocates `sequence` and `run_id` once, before fan-out, so
   pull consumers, sinks, persistence, and Swift receive identical identity.

**Decision: option 2, the owner review's preferred option, is selected.**
Option 1 is not the binding design. The selected contract makes sequence gaps
detectable and gives durable sessions the same authoritative identity as live
observation. This must be a canonical runtime change because the sequence must
be allocated once before the event branches to the observation channel and
sinks. A binding-only counter would create a second authority; the current
UniFFI layer does exactly that at `bindings/pi-ffi/src/lib.rs:703` and
`bindings/pi-ffi/src/lib.rs:730`.

The actor reads the event's sequence from the pre-apply
`snapshot.next_sequence`. On `RunStarted`, it takes and retains that event's
`run_id`; every later event in the run uses the retained identity. It applies
the event, which advances the snapshot sequence at
`crates/pi-agent-runtime-tokio/src/lib.rs:858`, and then constructs exactly one
envelope for both the observational sender and every sink. An event before
`RunStarted`, or one whose run identity conflicts with the active run, is a
`SnapshotInvariant` error rather than an invitation to invent an identity.
No consumer allocates or repairs sequence values.

Accordingly, every Agent pull and sink signature in this design carries
`AgentEventEnvelope`, and acceptance test 12 is unconditional. The embedded
`event` remains the existing canonical `AgentEvent`; there is no duplicate
envelope hierarchy.

### 4.6 Add concrete collection inputs and preserve owned values

`TokioAgentHandle::prompt_records` currently accepts generic
`impl IntoIterator<Item = AgentRecord>` and immediately collects a `Vec`
(`crates/pi-agent-runtime-tokio/src/lib.rs:230`,
`crates/pi-agent-runtime-tokio/src/lib.rs:232`,
`crates/pi-agent-runtime-tokio/src/lib.rs:234`). Change the canonical actor
method to accept `Vec<AgentRecord>` directly, or add a distinctly named
Rust-generic convenience that delegates to the concrete method. `Vec<T>` is a
documented boundary collection.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections]
The exported method must be the concrete one; no duplicate input record is
introduced.

`snapshot` and `latest_snapshot` already return owned `AgentSnapshot`
(`crates/pi-agent-runtime-tokio/src/lib.rs:349`,
`crates/pi-agent-runtime-tokio/src/lib.rs:364`). Steering and follow-up already
take owned `AgentRecord` and return owned `QueueReceipt`
(`crates/pi-agent-runtime-tokio/src/lib.rs:255`,
`crates/pi-agent-runtime-tokio/src/lib.rs:257`,
`crates/pi-agent-runtime-tokio/src/lib.rs:270`,
`crates/pi-agent-runtime-tokio/src/lib.rs:272`). Preserve those ordinary Rust
contracts.

### 4.7 Own the Tokio runtime

`TokioAgentHandle::with_capacities` currently calls
`tokio::runtime::Handle::try_current()` and returns `NoRuntime` if no runtime is
entered (`crates/pi-agent-runtime-tokio/src/lib.rs:178`,
`crates/pi-agent-runtime-tokio/src/lib.rs:183`). BoltFFI provides
future polling but no Rust executor and requires a runtime to be active for
Tokio-dependent libraries.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]

Add `TokioRuntimeOwner` to `pi-agent-runtime-tokio` as the concrete production
factory. It owns the runtime supervisor, a cloned concrete `Models` control
plane, a cloned `ToolRegistry`, and the actor capacities. `Models` is already a
cloneable concrete control-plane type and implements the narrow `ModelRuntime`
capability (`crates/pi-ai/src/models.rs:47`,
`crates/pi-ai/src/models.rs:48`,
`crates/pi-ai/src/models.rs:49`,
`crates/pi-ai/src/models.rs:1399`). `ToolRegistry` is already cloneable and has
an empty constructor (`crates/pi-agent-core/src/tools.rs:490`,
`crates/pi-agent-core/src/tools.rs:491`,
`crates/pi-agent-core/src/tools.rs:497`). `AgentState` is the existing owned
agent construction value (`crates/pi-agent-core/src/state.rs:23`). The factory
therefore needs no bare `Agent`, arbitrary trait object, JSON command, or
binding-only configuration record.

Add an inherent `Models::new() -> Self` that delegates to the existing
`Default` implementation (`crates/pi-ai/src/models.rs:99`,
`crates/pi-ai/src/models.rs:101`). This is the canonical empty control-plane
constructor for Rust and Swift, not a scripted runtime and not a complete
production configuration. BoltFFI documents class constructors as inherent
methods returning `Self`.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
Whether a `Default` trait implementation can itself generate a target-language
constructor is **UNRESOLVED: not answered by the documentation**; pages checked:
`classes.md#defining-a-class` and `classes.md#constructors`. This design does not
depend on that behavior.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]

#### First concrete production `Models` path: OpenAI with native HTTP and API-key auth

Empty `Models::new()` is insufficient for an ordinary Swift application. The
existing mutation accepts `ProviderRegistration`
(`crates/pi-ai/src/models.rs:399`), but that record contains
`Arc<dyn AuthResolver>`, `Arc<dyn ModelCatalog>`, a dispatch map of
`Arc<dyn ChatApi>`, and a retry-classifier trait object
(`crates/pi-ai/src/provider.rs:2320`,
`crates/pi-ai/src/provider.rs:2324`,
`crates/pi-ai/src/provider.rs:2326`,
`crates/pi-ai/src/provider.rs:2331`,
`crates/pi-ai/src/provider.rs:2335`). The current OpenAI factory still requires
`Arc<dyn HttpTransport>` (`providers/pi-ai-openai/src/handler.rs:487`,
`providers/pi-ai-openai/src/handler.rs:488`), whose execution contract is at
`crates/pi-ai/src/middleware.rs:275`. None of those authoring capabilities is a
foreign ordinary-consumer input. BoltFFI lists arbitrary `dyn Trait` among the
types that do not cross its ordinary value boundary.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]

Add one concrete, canonical provider factory to `pi-ai-openai`:

```rust
pub struct OpenAiModelsFactory {
    transport: Arc<NativeOpenAiHttpTransport>,
    api_key: SecretString,
}

#[error]
pub struct OpenAiModelsError {
    pub code: String,
    pub message: String,
}

#[export]
impl OpenAiModelsFactory {
    pub fn new(
        api_key: String,
    ) -> Result<Self, OpenAiModelsError>;

    pub fn build(
        &self,
    ) -> Result<Models, OpenAiModelsError>;
}
```

This is a normal Rust provider-construction API, not an FFI facade. Its fields
are private implementation state; BoltFFI's documented class form keeps the
object in Rust and exposes only methods in the annotated impl.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
The fallible `new` uses the documented fallible-constructor shape, `build`
returns another Rust-backed class using the documented class-valued method
shape, and the proposed two-string error uses the documented structured-error
shape.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#fallible-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors]

`NativeOpenAiHttpTransport` is a concrete production transport implemented in
`pi-ai-openai`; it implements the current `HttpTransport` request,
cancellation, established-response, and streaming-body contract
(`crates/pi-ai/src/middleware.rs:275`,
`crates/pi-ai/src/middleware.rs:277`,
`crates/pi-ai/src/middleware.rs:281`). It is not a Swift callback and is never a
generated parameter. `OpenAiModelsFactory::new` creates that concrete
transport and converts the supplied key immediately to the existing redacting
`SecretString` (`crates/pi-ai/src/auth.rs:24`,
`crates/pi-ai/src/auth.rs:26`,
`crates/pi-ai/src/auth.rs:29`). The factory must reject an empty key and report
only sanitized error text.

`build` returns the canonical `Models`, not an `OpenAiModels` wrapper. It:

1. Calls the existing `openai_provider` with the concrete transport coerced to
   its internal `Arc<dyn HttpTransport>` seam
   (`providers/pi-ai-openai/src/handler.rs:487`).
2. Seeds an `InMemoryCredentialStore` with the canonical
   `Credential::ApiKey(ApiKeyCredential)` for provider `openai`; those existing
   values are at `crates/pi-ai/src/auth.rs:52`,
   `crates/pi-ai/src/auth.rs:55`, `crates/pi-ai/src/auth.rs:208`, and
   `crates/pi-ai/src/auth.rs:212`.
3. Calls the existing `ModelsBuilder::credential_store`,
   `ModelsBuilder::provider`, and `ModelsBuilder::build`
   (`crates/pi-ai/src/models.rs:1476`,
   `crates/pi-ai/src/models.rs:1501`,
   `crates/pi-ai/src/models.rs:1541`).

The existing OpenAI registration installs its bearer resolver at
`providers/pi-ai-openai/src/handler.rs:500`; that resolver is built over the
standard environment/API-key auth at
`providers/pi-ai-openai/src/handler.rs:781` and
`providers/pi-ai-openai/src/handler.rs:785`. The canonical API-key resolver
checks a stored nonempty key before ambient environment lookup
(`crates/pi-ai/src/auth.rs:1485`,
`crates/pi-ai/src/auth.rs:1487`,
`crates/pi-ai/src/auth.rs:1494`). Thus the seeded credential is the auth path
used by requests from the returned `Models`; it is not a parallel provider
configuration.

Add `InMemoryCredentialStore::with_credential(provider, credential) -> Self`
beside its current empty constructor
(`crates/pi-ai/src/auth.rs:347`, `crates/pi-ai/src/auth.rs:349`) so this seed is
synchronous and atomic at construction. That helper remains ordinary Rust-only
for this milestone; neither `CredentialStore` nor its lease trait enters the
generated API. The resulting `Models` has the real OpenAI catalog, provider
auth resolver, native transport, and stored API key before it is handed to
`TokioRuntimeOwner`. It is usable by both `Agent` and direct
`Models::stream_simple` through the existing concrete control plane
(`crates/pi-ai/src/models.rs:762`, `crates/pi-ai/src/models.rs:1399`).

This first path is deliberately one provider and process-local credential
storage. Additional concrete providers and a file-backed credential factory can
be added later without exposing provider-authoring traits. The initial package
is not accepted unless the generated-language construction test in section 9
builds this factory, obtains its configured `Models`, spawns an actor, and
shuts that actor down without any test-only constructor.

The canonical ordinary-Rust API is:

```rust
pub struct TokioRuntimeOwner {
    supervisor: Arc<RuntimeSupervisor>,
    models: Models,
    tools: ToolRegistry,
    command_capacity: usize,
    event_capacity: usize,
}

#[export]
impl TokioRuntimeOwner {
    pub fn new(
        models: &Models,
        tools: &ToolRegistry,
    ) -> Result<Self, TokioAgentError>;

    pub fn spawn_agent(
        &self,
        state: AgentState,
    ) -> Result<TokioAgentHandle, TokioAgentError>;

    pub async fn stream_model(
        &self,
        request: ModelRequest,
    ) -> Result<TokioAssistantStream, RequestStartError>;
}

impl TokioRuntimeOwner {
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle; // Rust-only
}
```

`new` clones the supplied canonical objects. `spawn_agent` performs the exact
ordinary construction internally:

```rust
let agent = Agent::new(
    Arc::new(self.models.clone()),
    state,
    self.tools.clone(),
)?;
```

That is the current native constructor contract
(`crates/pi-agent-core/src/run.rs:140`) with the concrete `Models`
implementation selected instead of exposing `Arc<dyn ModelRuntime>`. A no-tool
Swift application supplies `ToolRegistry::new`; the later Swift-authored-tool
milestone populates the same canonical registry. The first production Swift
application obtains its configured canonical `Models` from
`OpenAiModelsFactory::build` above. Provider implementation traits remain
unannotated, and the Tokio factory neither knows how to construct OpenAI nor
duplicates provider configuration. `Models` remains the provider/auth/catalog
control plane (`crates/pi-ai/src/models.rs:399`,
`crates/pi-ai/src/models.rs:762`).

BoltFFI documents borrowed Rust-backed classes as method/constructor arguments,
so `&Models` and `&ToolRegistry` are the intended generated-class inputs once
those two canonical types have annotated class impls.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
It also documents fallible constructors as target-language throwing
constructors.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#fallible-constructors]
Whether an *owned* Rust-backed class argument would be accepted remains
**UNRESOLVED: not answered by the documentation**; pages checked:
`functions.md#classes`, `classes.md#constructors`, and
`classes.md#methods-that-take-or-return-classes`. The design does not require
that form.
[https://www.boltffi.dev/docs/functions.md | docs/boltffi-swift-bindings/docs-snapshot/functions.md#classes]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]

Runtime lifetime is a task invariant, not merely a handle field. The runtime
supervisor owns the actual `tokio::runtime::Runtime` on its native owner thread
and routes runtime-owned spawning through `spawn_with_lease`. The actor future
and every direct-model producer capture a counted `RuntimeLease` before polling
user work and release it only after `actor_loop` or the producer exits. Tool,
provider-transport, or environment work polled inside the actor is covered by
the actor lease; any such work deliberately detached from that future must
acquire its own lease through the same supervisor. Dropping
`TokioRuntimeOwner` marks the supervisor for shutdown, but dropping that owner
or every external `TokioAgentHandle` cannot stop the runtime while a leased task
is live. The supervisor's native owner thread drops the actual runtime only
after shutdown was requested and the last task lease settled.

The current `shutdown` acknowledgement is sent immediately before the idle
actor returns (`crates/pi-agent-runtime-tokio/src/lib.rs:688`), or after an
active `drive_run` settles (`crates/pi-agent-runtime-tokio/src/lib.rs:718`). Add
a reusable actor-done signal owned by `TokioAgentHandle`; reshaped
`shutdown(&self)` first obtains the mailbox acknowledgement and then awaits that
done signal. This makes successful shutdown mean the actor future has released
its runtime lease, not merely that it has accepted the shutdown command. Add a
cloneable `TokioAgentError::RuntimeInitialization { message: String }` variant
for supervisor/runtime-builder failure; the current error has only `NoRuntime`
for missing ambient construction (`crates/pi-agent-runtime-tokio/src/lib.rs:72`).

### 4.8 Add the concrete direct `AssistantEvent` pull object

Add `TokioAssistantStream` and `TokioAssistantError` to the same canonical
Tokio crate. The current `AssistantStream` owns a `Send + 'static` event stream
and fuses on terminal event or EOF (`crates/pi-ai/src/streaming.rs:1901`,
`crates/pi-ai/src/streaming.rs:1934`). The
Tokio object owns the foreign-consumer delivery state without replacing
`AssistantEvent`:

```rust
pub struct TokioAssistantStream {
    events: tokio::sync::Mutex<mpsc::Receiver<AssistantEvent>>,
    completion:
        watch::Receiver<Option<Result<(), TokioAssistantError>>>,
    cancellation: CancellationToken,
    // Private state records observation closure and terminal delivery.
}

#[export]
impl TokioAssistantStream {
    pub async fn next_event(
        &self,
    ) -> Result<Option<AssistantEvent>, TokioAssistantError>;

    pub fn cancel(&self);

    pub async fn cancel_and_wait(
        &self,
    ) -> Result<(), TokioAssistantError>;
}

impl TokioAssistantStream {
    pub fn events(
        &mut self,
    ) -> &mut mpsc::Receiver<AssistantEvent>; // Rust-only
}
```

`TokioRuntimeOwner::stream_model` creates one token and bounded event channel,
then spawns a producer under a runtime lease. The producer first calls the
stored concrete `Models::stream_simple` and completes an establishment
handshake; only a successfully established `AssistantStream` is returned to
the caller (`crates/pi-ai/src/models.rs:762`). The exported future awaits only
that channel handshake, which is compatible with the documentation's statement
that channel-based async does not itself need a runtime; the provider work runs
on the Rust-owned runtime because Tokio-dependent libraries require an active
runtime.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]

The establishment await owns a drop guard containing the receive side and one
token clone. BoltFFI documents that target-language cancellation marks and
wakes the exported Rust future and that cleanup then runs through the generated
future's `free` operation.
[https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]
The guard is therefore the proposed Rust cleanup point when Swift cancels before
the handshake completes: it closes the observation channel and cancels the
model token so the producer can settle and release its runtime lease. The guard
behavior is part of the proposed canonical Rust factory, not an additional
BoltFFI guarantee.

After establishment, the producer polls each `AssistantEvent` on the owned
runtime and awaits bounded `mpsc::Sender::send`; this is lossless backpressure,
not a BoltFFI ring. `next_event(&self)` serializes receiver access, returns each
event unchanged, records terminal delivery, and validates cached producer
completion at EOF. It returns `Ok(None)` only after delivering
`Finished`, `Failed`, or `Cancelled`; raw EOF first yields
`TokioAssistantError::MissingTerminalEvent`, and producer/supervisor loss yields
`TokioAssistantError::Closed`. The current event variants and terminal test are
at `crates/pi-ai/src/streaming.rs:521`,
`crates/pi-ai/src/streaming.rs:528`,
`crates/pi-ai/src/streaming.rs:534`, and
`crates/pi-ai/src/streaming.rs:540`.

`cancel()` cancels only this model call and preserves terminal lifecycle
delivery when the caller keeps pulling. `cancel_and_wait()` cancels the token,
closes/discards the observational receiver, wakes a producer blocked on channel
capacity, and awaits cached completion. Once observation is closed, the
producer continues draining internally until it sees and validates the in-band
terminal event; it does not report intentional observation closure as
`MissingTerminalEvent`. Cancellation or provider failure after establishment
remains an in-band terminal event under the current runtime contract
(`crates/pi-ai/src/runtime.rs:42`). Concurrent pulls serialize on the receiver
mutex. Raw receiver access stays in an unannotated impl.

Exported async `&self` methods, throwing async results, and default
`Send + Sync` class checking are documented BoltFFI class forms.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
No `#[ffi_stream]` attribute is used because that documented attribute requires
`Arc<EventSubscription<T>>`, whose finite ring drops new events when full.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]

## 5. In-scope mapping table

“Direct after canonical change” means the signature has a documented BoltFFI
shape once every transitive value type maps. Named-field records and the
documented unit or struct-style associated-data enums use `#[data]`;
Rust-owned classes use `#[export]`; async `Result` methods map to Swift
`async throws`. Tuple-style data-enum variants are not shown by the records
documentation and remain an explicit generation gate below.
**UNRESOLVED: not answered by the documentation**. Pages checked:
`records.md#enums`, `records.md#enums-with-associated-data`, and
`types.md#records`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]

| Ordinary-consumer surface | Current source | Planned mapping and status |
|---|---|---|
| `TokioRuntimeOwner::{new,spawn_agent,stream_model}` | `TokioAgentHandle::with_capacities` currently calls `Handle::try_current()` at `crates/pi-agent-runtime-tokio/src/lib.rs:183`; current `Agent::new` takes runtime/state/tools at `crates/pi-agent-core/src/run.rs:140` | Add section 4.7's canonical factory over borrowed concrete `Models` and `ToolRegistry`, owned `AgentState`, and a supervised runtime. The actor and direct-model producer each capture a task lease. Raw Tokio handles stay unannotated. Exported classes and borrowed class arguments are documented. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] |
| Concrete `Models` input | Cloneable control plane at `crates/pi-ai/src/models.rs:47`, `crates/pi-ai/src/models.rs:48`, and `crates/pi-ai/src/models.rs:49`; current empty `Default` construction at `crates/pi-ai/src/models.rs:99`; concrete `ModelRuntime` impl at `crates/pi-ai/src/models.rs:1399` | Add and annotate inherent `Models::new() -> Self` as an explicitly empty constructor. Production Swift uses the next row's concrete OpenAI factory, then passes the returned canonical `Models` to `TokioRuntimeOwner`; it never supplies `ProviderRegistration` or `ModelRuntime`. Rust-backed classes and inherent constructors are documented. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] |
| `OpenAiModelsFactory::{new,build}` | Existing `openai_provider` requires `Arc<dyn HttpTransport>` at `providers/pi-ai-openai/src/handler.rs:487`; `ProviderRegistration` contains provider/auth/catalog trait objects at `crates/pi-ai/src/provider.rs:2320`; `ModelsBuilder` accepts the registration and credential store at `crates/pi-ai/src/models.rs:1476`, `crates/pi-ai/src/models.rs:1501`, and `crates/pi-ai/src/models.rs:1541` | Add section 4.7's canonical concrete class in `pi-ai-openai`. It owns `NativeOpenAiHttpTransport` and a redacted API key in Rust, and `build()` returns the canonical configured `Models`. No provider/transport/auth trait is a generated parameter. Private Rust class state, fallible constructors, class-valued methods, and structured errors are documented. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#fallible-constructors] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#struct-errors] |
| Concrete `ToolRegistry` input | Cloneable registry at `crates/pi-agent-core/src/tools.rs:490`, `crates/pi-agent-core/src/tools.rs:491`; empty constructor at `crates/pi-agent-core/src/tools.rs:497` | Annotate the canonical class constructor and safe observations first; registration of Swift-authored `Tool` implementations remains a callback-authoring milestone. The same object is cloned into production Agent construction. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors] |
| `TokioAgentHandle` | Class at `crates/pi-agent-runtime-tokio/src/lib.rs:158` | Direct class after the runtime-retaining constructor and transitive types map. Only the concrete actor impl is annotated. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class] |
| `prompt_text`, `continue_run`, `retry_last_turn` | `crates/pi-agent-runtime-tokio/src/lib.rs:203`, `crates/pi-agent-runtime-tokio/src/lib.rs:243`, `crates/pi-agent-runtime-tokio/src/lib.rs:249`; shared current establishment helper at `crates/pi-agent-runtime-tokio/src/lib.rs:394` | Direct async throwing class methods returning reshaped `TokioAgentRun`. Section 4.1's armed establishment guard makes cancellation before return cancel the already accepted shared run token and close unclaimed observation. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods] [https://www.boltffi.dev/docs/async.md \| docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] [https://www.boltffi.dev/docs/async.md \| docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation] [https://www.boltffi.dev/docs/async-internals.md \| docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation] |
| `prompt_records` | Generic collection at `crates/pi-agent-runtime-tokio/src/lib.rs:230` | Canonical concrete `Vec<AgentRecord>` input, then direct async throwing method. `Vec` is documented. [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#collections] |
| `prompt_text_with_sink`, `subscribe`, `unsubscribe` | `crates/pi-agent-runtime-tokio/src/lib.rs:217`, `crates/pi-agent-runtime-tokio/src/lib.rs:306`, `crates/pi-agent-runtime-tokio/src/lib.rs:319` | Conditional on the async-trait rewrite and owned callback-argument generation test. Every run-scoped or registered sink receives `AgentEventEnvelope`. `EventSinkId` tuple-newtype mapping remains unresolved below. Async stored host traits and `Arc` ownership are documented separately. [https://www.boltffi.dev/docs/callbacks.md \| docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/callbacks.md \| docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#storing-traits] |
| `steer`, `follow_up`, handle `cancel`, `cancel_now` | `crates/pi-agent-runtime-tokio/src/lib.rs:255`, `crates/pi-agent-runtime-tokio/src/lib.rs:270`, `crates/pi-agent-runtime-tokio/src/lib.rs:285`, `crates/pi-agent-runtime-tokio/src/lib.rs:301` | Direct async/sync methods after `AgentRecord`, `QueueReceipt`, `RunId`, and errors map. These remain distinct from run-local `TokioAgentRun::cancel`. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods] |
| `reset_transcript`, `reset_all`, `snapshot`, `latest_snapshot`, `wait_for_idle`, `shutdown` | `crates/pi-agent-runtime-tokio/src/lib.rs:329`, `crates/pi-agent-runtime-tokio/src/lib.rs:339`, `crates/pi-agent-runtime-tokio/src/lib.rs:349`, `crates/pi-agent-runtime-tokio/src/lib.rs:364`, `crates/pi-agent-runtime-tokio/src/lib.rs:374`, `crates/pi-agent-runtime-tokio/src/lib.rs:385` | Direct async/sync methods after snapshot/error mapping. `latest_snapshot` is already owned; `shutdown` gains actor-done settlement from section 4.7. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods] |
| `snapshots()` | Raw watch receiver at `crates/pi-agent-runtime-tokio/src/lib.rs:369` | Keep unannotated. It is a raw Tokio receiver, and the documented stream attribute requires `Arc<EventSubscription<T>>`. [https://www.boltffi.dev/docs/streaming.md \| docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute] |
| `TokioAgentRun::{next_event,outcome,cancel,cancel_and_outcome}` | Current run at `crates/pi-agent-runtime-tokio/src/lib.rs:126` | Direct class after section 4.1. The selected pull is `Result<Option<AgentEventEnvelope>, TokioAgentError>`. Async `Result<Option<T>, E>` composes documented async, error, and optional mappings. [https://www.boltffi.dev/docs/async.md \| docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#option] |
| `TokioAgentRun::events` | `crates/pi-agent-runtime-tokio/src/lib.rs:133` | Keep in a separate unannotated Rust-only impl. No `EventSubscription` adapter is added. |
| `TokioAssistantStream::{next_event,cancel,cancel_and_wait}` | Raw `AssistantStream` at `crates/pi-ai/src/streaming.rs:1900`; direct `Models::stream_simple` at `crates/pi-ai/src/models.rs:762` | Add the canonical Rust-owned Tokio pull object in section 4.8. Start errors throw before the object is returned; established completion stays in-band; premature EOF and producer loss throw from `next_event`. No `EventSubscription` adapter is added. Async throwing and optional mappings are documented. [https://www.boltffi.dev/docs/async.md \| docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#option] |
| `TokioAssistantStream::events` | No current concrete owner; raw `AssistantStream` implements `Stream` at `crates/pi-ai/src/streaming.rs:1934` | Keep the added raw Tokio receiver accessor in a separate unannotated Rust-only impl. |
| `AgentEventSink` | Boxed future trait carrying bare `AgentEvent` at `crates/pi-agent-runtime-tokio/src/lib.rs:45`, `crates/pi-agent-runtime-tokio/src/lib.rs:49` | Rewrite to the documented async-trait form with `AgentEventEnvelope`; preserve `Arc<dyn AgentEventSink>`, ordering, and acknowledgement. Owned `CancellationToken` callback input is unresolved pending generation. [https://www.boltffi.dev/docs/callbacks.md \| docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md \| docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] |
| `PromptImage`, `PromptText` | `crates/pi-agent-core/src/run.rs:84`; `crates/pi-agent-core/src/run.rs:93` | Direct data: strings and `Vec<PromptImage>` are documented record/collection fields. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#structs-with-strings-or-collections] |
| `ModelRequest`, `ModelRef`, `Context`, `SimpleGenerationOptions` | `crates/pi-ai/src/runtime.rs:13`, `crates/pi-ai/src/ids.rs:105`, `crates/pi-ai/src/messages.rs:467`, `crates/pi-ai/src/options.rs:561` | These are the canonical owned direct-model inputs; no command envelope replaces them. `ModelRequest` is a record only after its entire nested graph maps. In addition to tuple-string IDs, `OrderedJsonObject` and its recursive `OrderedJsonString`/`OrderedJsonArray` values, `HeaderMapSpec`, and `ErasedApiOptionsPatch::value`, `Context.messages` reaches assistant `ToolCall.arguments: serde_json::Value`, `DeferredHandle.data: serde_json::Value`, diagnostic `serde_json::Number`/`BTreeMap<String, serde_json::Value>`, `Timestamp`, `Currency` through terminal cost, and `ToolResultMessage.details -> VersionedExtension.value: RawValue`; `Context.tools` reaches `ToolSpec.parameters: serde_json::Value` and `GrammarVariants: BTreeMap<GrammarFormat, String>` (`crates/pi-ai/src/runtime.rs:17`, `crates/pi-ai/src/options.rs:598`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/messages.rs:143`, `crates/pi-ai/src/messages.rs:152`, `crates/pi-ai/src/messages.rs:169`, `crates/pi-ai/src/messages.rs:182`, `crates/pi-ai/src/messages.rs:217`, `crates/pi-ai/src/messages.rs:233`, `crates/pi-ai/src/messages.rs:306`, `crates/pi-ai/src/messages.rs:319`, `crates/pi-ai/src/messages.rs:418`, `crates/pi-ai/src/messages.rs:473`, `crates/pi-ai/src/messages.rs:475`, `crates/pi-ai/src/deferred.rs:39`, `crates/pi-ai/src/model.rs:923`, `crates/pi-ai/src/usage.rs:121`). Every path is a separate section 8.2 generation gate. Nested records are documented only when their field graph maps. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| `AgentEventEnvelope`, embedded `AgentEvent`, `TurnOutcome`, `RunOutcome` | `crates/pi-agent-core/src/events.rs:329`, `crates/pi-agent-core/src/events.rs:335`, `crates/pi-agent-core/src/events.rs:81`, `crates/pi-agent-core/src/events.rs:31`, `crates/pi-agent-core/src/events.rs:51` | The envelope is the selected canonical pull/sink record; the embedded event and outcomes remain the existing canonical payload types. The graph is blocked by section 8's tuple IDs plus the non-ID `Timestamp`, `Currency`, and `ReplayDropReason` tuple newtypes, `Arc<[T]>`, `i128` cost, and `BTreeSet<ModelFingerprint>`. `ReplayDropReason` is reachable through `AgentEvent::ContextPrepared -> HandoffReport.changes -> HandoffChange::OpaqueReplayDropped` (`crates/pi-agent-core/src/events.rs:97`, `crates/pi-agent-core/src/events.rs:103`, `crates/pi-ai/src/handoff.rs:84`, `crates/pi-ai/src/handoff.rs:92`, `crates/pi-ai/src/handoff.rs:150`). JSON-bearing paths are `ToolCall.arguments` in `ToolExecutionStarted`; `ToolUpdate.details: RawValue` in `ToolExecutionUpdated`; `ToolOutput.details: RawValue` in `ToolExecutionFinished`; and assistant/tool-result JSON reachable through explicit `AssistantUpdate` and `MessageCommitted` roots (`crates/pi-agent-core/src/events.rs:113`, `crates/pi-agent-core/src/events.rs:120`, `crates/pi-agent-core/src/events.rs:125`, `crates/pi-agent-core/src/events.rs:130`, `crates/pi-agent-core/src/events.rs:137`, `crates/pi-agent-core/src/tools.rs:51`, `crates/pi-agent-core/src/tools.rs:93`). `AgentEvent` is also `#[non_exhaustive]`; section 8.4 gates generation because the documentation does not describe that attribute. Payload enums themselves are documented. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] |
| `AssistantEvent` and its replay/message graph | Non-exhaustive enum at `crates/pi-ai/src/streaming.rs:357`, `crates/pi-ai/src/streaming.rs:360`; replay-event roots at `crates/pi-ai/src/streaming.rs:474`, `crates/pi-ai/src/streaming.rs:488`; nested variant/field at `crates/pi-agent-core/src/events.rs:113`, `crates/pi-agent-core/src/events.rs:117` | Export the same owned enum both directly through `TokioAssistantStream` and nested in `AgentEvent::AssistantUpdate`; never as `EventSubscription<AssistantEvent>`. In addition to the diagnostic/JSON/message paths below, every terminal variant carries `AssistantMessage.replay: ReplayEnvelope`; the envelope contains `ReplayScope` and ordered `Vec<ReplayItem>`, and each item contains `ReplayTarget` and `OpaquePayload` (`crates/pi-ai/src/messages.rs:157`, `crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:17`, `crates/pi-ai/src/replay.rs:19`, `crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:141`, `crates/pi-ai/src/replay.rs:149`). Generation/fidelity must cover nonempty items, all four `ReplayTarget` forms, and `OpaquePayload::{Utf8,Bytes,JsonBytes}`. Independently, standalone `AssistantEvent::ReplayItemStarted` must cover all target forms and standalone `AssistantEvent::ReplayData` must cover all five `ReplayDataOperation` forms; every one is mirrored inside `AgentEvent::AssistantUpdate` with distinct sentinels (`crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`, `crates/pi-ai/src/streaming.rs:563`). All three terminal variants also carry `AssistantMessage` paths for `ToolCall.arguments`, `DeferredHandle.data`, diagnostics, `Timestamp`, and `Currency` (`crates/pi-ai/src/messages.rs:143`, `crates/pi-ai/src/messages.rs:152`, `crates/pi-ai/src/messages.rs:155`, `crates/pi-ai/src/messages.rs:165`, `crates/pi-ai/src/messages.rs:169`, `crates/pi-ai/src/streaming.rs:521`, `crates/pi-ai/src/streaming.rs:527`, `crates/pi-ai/src/streaming.rs:533`). Each direct and nested route is its own gate. `AssistantEvent`'s `#[non_exhaustive]` attribute and the tuple-style variants inside its graph are separate unresolved gates in sections 8.4 and 8.2. The records documentation demonstrates struct-style associated data but not tuple-style variant syntax. **UNRESOLVED: not answered by the documentation**. Pages checked: `records.md#enums`, `records.md#enums-with-associated-data`, and `types.md#records`. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/streaming.md \| docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity] |
| `AgentState`, `AgentRecord`, `AgentSnapshot` | `crates/pi-agent-core/src/state.rs:23`, `crates/pi-agent-core/src/state.rs:62`, `crates/pi-agent-core/src/state.rs:180` | Owned canonical data with two independent nested roots. `AgentSnapshot.state` reaches the committed transcript: `AgentRecord::Custom.payload: RawValue`; assistant content, deferred data, diagnostics, timestamps/costs, tool-result details, and each committed assistant's complete `AssistantMessage.replay` graph (`crates/pi-agent-core/src/state.rs:33`, `crates/pi-agent-core/src/state.rs:64`, `crates/pi-agent-core/src/state.rs:70`, `crates/pi-agent-core/src/state.rs:184`, `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:38`, `crates/pi-ai/src/messages.rs:143`, `crates/pi-ai/src/messages.rs:152`, `crates/pi-ai/src/messages.rs:157`, `crates/pi-ai/src/messages.rs:165`, `crates/pi-ai/src/messages.rs:169`, `crates/pi-ai/src/messages.rs:233`). Independently, `AgentSnapshot.streaming: Option<AssistantMessageSnapshot>` reaches partial deferred/diagnostics/content plus `replay: ReplayEnvelope`, usage/cost, timestamp, and optional terminal `AssistantMessage` (`crates/pi-agent-core/src/state.rs:188`, `crates/pi-ai/src/streaming.rs:1689`, `crates/pi-ai/src/streaming.rs:1693`, `crates/pi-ai/src/streaming.rs:1695`, `crates/pi-ai/src/streaming.rs:1697`, `crates/pi-ai/src/streaming.rs:1699`, `crates/pi-ai/src/streaming.rs:1701`, `crates/pi-ai/src/streaming.rs:1703`, `crates/pi-ai/src/streaming.rs:1705`). Both `streaming.replay` and `streaming.terminal_message.replay` are separate complete roots: each must have its own distinct `ReplayScope`, nonempty ordered items, all four `ReplayTarget` forms, and all three `OpaquePayload` forms (`crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:17`, `crates/pi-ai/src/replay.rs:19`, `crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:141`, `crates/pi-ai/src/replay.rs:149`, `crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`). `pending_tool_calls` separately reaches `Arc<[ToolCallId]>` (`crates/pi-agent-core/src/state.rs:190`). Do not create Swift-specific snapshot or transcript records. Nested records are documented only when the full graph maps. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#nested-structs] |
| Transitive tuple-newtype values: IDs, `QueueSequence`, `EventSinkId`, `Timestamp`, `Currency`, `ReplayDropReason`, `OrderedJsonString`, `OrderedJsonObject`, `OrderedJsonArray` | Macro IDs at `crates/pi-ai/src/ids.rs:6`; other definitions at `crates/pi-agent-core/src/control.rs:19`, `crates/pi-agent-runtime-tokio/src/lib.rs:35`, `crates/pi-ai/src/ids.rs:133`, `crates/pi-ai/src/usage.rs:88`, `crates/pi-ai/src/handoff.rs:44`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227` | Every listed type is an in-scope value root or a transitive field in the request/event/outcome/snapshot/control graph. The documentation shows named-field structs but does not establish tuple-newtype generation. **UNRESOLVED: not answered by the documentation**. Pages checked: `records.md#structs`, `types.md#records`, and `custom-types.md#representation-types`. Each type therefore receives a separate generation and exact-fidelity test; success for tuple IDs cannot be generalized to the non-ID wrappers or to the recursively nested ordered-JSON wrappers. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#records] [https://www.boltffi.dev/docs/custom-types.md \| docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] |
| Tuple-payload data enums: `Message`, `AgentRecord`, `DiagnosticErrorCode`, `ConstrainedSampling`, `OrderedJsonValue`, `ReplayTarget`, `OpaquePayload`, `ReplayDataOperation` | Definitions at `crates/pi-ai/src/messages.rs:32`, `crates/pi-agent-core/src/state.rs:62`, `crates/pi-ai/src/messages.rs:178`, `crates/pi-ai/src/messages.rs:334`, `crates/pi-ai/src/json_compat.rs:324`, `crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`, and `crates/pi-ai/src/streaming.rs:563` | Each canonical enum has at least one tuple-style variant and is independently reachable through the ordinary request/event/snapshot/control surface. The records page demonstrates unit variants and struct-style associated-data variants, but not tuple-style syntax. **UNRESOLVED: not answered by the documentation**. Pages checked: `records.md#enums`, `records.md#enums-with-associated-data`, and `types.md#records`. Generate and round-trip every variant of every listed enum through Swift; a success for one enum or payload type is not evidence for another. Do not substitute a binding-only enum or envelope. [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#records] |
| `QueueSequence`, `QueueKind`, `QueueDrainMode`, `QueueReceipt`, `ControlError` | `crates/pi-agent-core/src/control.rs:19`, `crates/pi-agent-core/src/control.rs:27`, `crates/pi-agent-core/src/control.rs:37`, `crates/pi-agent-core/src/control.rs:58`, `crates/pi-agent-core/src/control.rs:68`; `ControlError::QueueFull.capacity: usize` at `crates/pi-agent-core/src/control.rs:74` | Enums/record/error are candidates, but three independent gates apply: `QueueSequence` tuple-newtype generation, `ControlError`'s `#[non_exhaustive]` handling, and exact `usize` payload generation for `QueueFull.capacity`. The numeric quick-reference table lists fixed-width integers but not `usize`; the isolated `usize` function-argument example does not answer error-payload support. **UNRESOLVED: not answered by the documentation**. Pages checked: `types.md#quick-reference`, `types.md#primitives`, `records.md#enums`, `errors.md#enum-errors`, and `errors.md#enums-with-payloads`. Acceptance test 18 must catch `QueueFull` in generated Swift and prove exact capacity fidelity; success of the tuple-newtype or non-exhaustive probe cannot discharge this gate. [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives] [https://www.boltffi.dev/docs/records.md \| docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads] |
| `CancellationToken` | Class at `crates/pi-ai/src/cancellation.rs:28`; `new`, `cancel`, `is_cancelled`, `check`, `cancelled`, and `child` at `crates/pi-ai/src/cancellation.rs:47`, `crates/pi-ai/src/cancellation.rs:62`, `crates/pi-ai/src/cancellation.rs:67`, `crates/pi-ai/src/cancellation.rs:72`, `crates/pi-ai/src/cancellation.rs:81`, `crates/pi-ai/src/cancellation.rs:90` | Rust-backed class for `new`, `cancel`, `is_cancelled`, `check`, and `child`; keep borrowed `cancelled()` future unannotated or skipped. Class-valued returns and skipped class methods are documented. [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] [https://www.boltffi.dev/docs/classes.md \| docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] |
| `TokioAgentError`, added `TokioAssistantError`, `RequestStartError`, `RequestStartErrorKind`, `AgentError`, `ControlError`, `CancellationError` | Current errors at `crates/pi-agent-runtime-tokio/src/lib.rs:70`, `crates/pi-ai/src/runtime.rs:23`, `crates/pi-ai/src/runtime.rs:48`, `crates/pi-agent-core/src/error.rs:14`, `crates/pi-agent-core/src/control.rs:68`, `crates/pi-ai/src/cancellation.rs:12`; `ControlError::QueueFull.capacity: usize` at `crates/pi-agent-core/src/control.rs:74`; `TokioAssistantError` is added in section 4.8 | Error candidates only after their complete payload graphs map. Current `TokioAgentError::Agent(AgentError)` is a tuple-payload error variant containing another error type (`crates/pi-agent-runtime-tokio/src/lib.rs:76`). The documentation establishes unit and struct-style payload error variants, but it does not establish either tuple-payload error variants or nested error-valued payloads; both are **UNRESOLVED: not answered by the documentation** after checking `errors.md#supported-error-types`, `errors.md#enum-errors`, and `errors.md#enums-with-payloads`. Section 8.3 is the gate. `RequestStartError.kind` contains non-exhaustive `RequestStartErrorKind` (`crates/pi-ai/src/runtime.rs:50`); it and all other in-scope `#[non_exhaustive]` occurrences are separately unresolved in section 8.4. `ControlError::QueueFull.capacity` adds a third, independent `usize` payload gate: the numeric quick-reference table does not list `usize`, and the isolated function-argument example does not establish error payload fields. **UNRESOLVED: not answered by the documentation**. Pages checked: `types.md#quick-reference`, `types.md#primitives`, and `errors.md#enums-with-payloads`. [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads] [https://www.boltffi.dev/docs/errors.md \| docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives] |
| `DEFAULT_COMMAND_CAPACITY`, `DEFAULT_EVENT_CAPACITY` | Both are `usize` constants at `crates/pi-agent-runtime-tokio/src/lib.rs:30` and `crates/pi-agent-runtime-tokio/src/lib.rs:33` | Keep both unannotated in the initial binding. The constants page permits supported result types but does not enumerate `usize`; the numeric quick-reference table omits `usize`, and its isolated function-argument example does not establish `usize` constants. **UNRESOLVED: not answered by the documentation**. Pages checked: `constants.md#supported-values`, `types.md#quick-reference`, and `types.md#primitives`. Rust contract tests may use the constants internally; generated Swift must not depend on exported capacity constants. [https://www.boltffi.dev/docs/constants.md \| docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md \| docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives] |

Replay-bearing values in the table have the following independent envelope
roots. This is part of the surface mapping, not a test-fixture shortcut:

1. `ModelRequest.context.messages -> Message::Assistant ->
   AssistantMessage.replay` (`crates/pi-ai/src/runtime.rs:17`,
   `crates/pi-ai/src/messages.rs:473`, `crates/pi-ai/src/messages.rs:36`,
   `crates/pi-ai/src/messages.rs:157`).
2. Direct `AssistantEvent::Finished.message.replay`
   (`crates/pi-ai/src/streaming.rs:521`,
   `crates/pi-ai/src/messages.rs:157`).
3. Direct `AssistantEvent::Failed.message.replay`
   (`crates/pi-ai/src/streaming.rs:528`,
   `crates/pi-ai/src/messages.rs:157`).
4. Direct `AssistantEvent::Cancelled.message.replay`
   (`crates/pi-ai/src/streaming.rs:534`,
   `crates/pi-ai/src/messages.rs:157`).
5. `AgentEvent::AssistantUpdate` carrying `Finished`, through its terminal
   message replay (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:521`,
   `crates/pi-ai/src/messages.rs:157`).
6. `AgentEvent::AssistantUpdate` carrying `Failed`, through its terminal
   message replay (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:528`,
   `crates/pi-ai/src/messages.rs:157`).
7. `AgentEvent::AssistantUpdate` carrying `Cancelled`, through its terminal
   message replay (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:534`,
   `crates/pi-ai/src/messages.rs:157`).
8. `AgentEvent::MessageCommitted` carrying
   `AgentRecord::Llm(Message::Assistant(_))`, through that assistant's replay
   (`crates/pi-agent-core/src/events.rs:120`,
   `crates/pi-agent-core/src/state.rs:64`,
   `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
9. A standalone `AgentRecord::Llm(Message::Assistant(_))`
   (`crates/pi-agent-core/src/state.rs:64`,
   `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
10. A direct `AgentState.transcript` assistant record
    (`crates/pi-agent-core/src/state.rs:33`,
    `crates/pi-agent-core/src/state.rs:64`,
    `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
11. `AgentSnapshot.state.transcript` containing an assistant record
    (`crates/pi-agent-core/src/state.rs:184`,
    `crates/pi-agent-core/src/state.rs:33`,
    `crates/pi-agent-core/src/state.rs:64`,
    `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
12. `AgentSnapshot.streaming.replay`
    (`crates/pi-agent-core/src/state.rs:188`,
    `crates/pi-ai/src/streaming.rs:1697`).
13. `AgentSnapshot.streaming.terminal_message.replay`
    (`crates/pi-agent-core/src/state.rs:188`,
    `crates/pi-ai/src/streaming.rs:1705`,
    `crates/pi-ai/src/messages.rs:157`).

Every numbered root is a separate mapping gate. It must carry a distinct,
nonempty `ReplayEnvelope`: the exact canonical `schema_version`, a complete
`ReplayScope` with root-specific sentinels, and an ordered item sequence with
root-specific IDs,
ordinals, kinds, applicability, and completeness; all four
`ReplayTarget::{Message,ContentBlock,ToolCall,ProviderOutputItem}` forms; and all
three `OpaquePayload::{Utf8,Bytes,JsonBytes}` forms with exact root-specific
strings and bytes (`crates/pi-ai/src/replay.rs:13`,
`crates/pi-ai/src/replay.rs:15`, `crates/pi-ai/src/replay.rs:17`,
`crates/pi-ai/src/replay.rs:19`, `crates/pi-ai/src/replay.rs:88`,
`crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:179`,
`crates/pi-ai/src/replay.rs:276`). The direct and nested
`AssistantEvent::{ReplayItemStarted,ReplayData}` matrices are additional event
roots and do not discharge any of these thirteen envelope roots
(`crates/pi-ai/src/streaming.rs:474`,
`crates/pi-ai/src/streaming.rs:488`,
`crates/pi-agent-core/src/events.rs:113`).

## 6. Cancellation semantics

Run establishment has a cancellation boundary before the two established-run
layers below. `prompt_text`, `prompt_text_with_sink`, `prompt_records`,
`continue_run`, and `retry_last_turn` all enter the shared `request_run` helper
(`crates/pi-agent-runtime-tokio/src/lib.rs:203`,
`crates/pi-agent-runtime-tokio/src/lib.rs:217`,
`crates/pi-agent-runtime-tokio/src/lib.rs:230`,
`crates/pi-agent-runtime-tokio/src/lib.rs:243`,
`crates/pi-agent-runtime-tokio/src/lib.rs:249`,
`crates/pi-agent-runtime-tokio/src/lib.rs:394`). Before actor acceptance,
dropping the establishment future closes `accepted_rx`; the actor's current
`accept_run` checks that closure before it marks the run active
(`crates/pi-agent-runtime-tokio/src/lib.rs:697`,
`crates/pi-agent-runtime-tokio/src/lib.rs:701`). After acceptance is sent, that
check is over: the actor may be in `drive_run` while the exported future is
awake but has not yet returned a `TokioAgentRun`
(`crates/pi-agent-runtime-tokio/src/lib.rs:705`,
`crates/pi-agent-runtime-tokio/src/lib.rs:523`).

Cancelling the Swift task during that accepted-but-unhanded interval
cooperatively cancels the exported future, and BoltFFI's documented async
cleanup proceeds through `free`.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
[https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]
Section 4.1's armed `RunEstablishmentGuard` must therefore remain alive until
the non-awaiting return step moves its receiver, reusable completion receiver,
and cancellation clone into `TokioAgentRun`. If `free` drops the future first,
the guard closes observation and explicitly cancels the shared token. Actor
acceptance is not ownership handoff. The actor then settles the in-band
cancellation lifecycle and sink barriers and publishes idle without relying on
a returned run object; orderly shutdown must subsequently let the actor release
its runtime lease. This is mandatory for every exported operation routed
through `request_run`, and test 19 exercises each prompt/continue/retry path at
the exact boundary.

For an established `TokioAgentRun`, there are two independent cancellation
layers.

1. Cancelling a Swift task awaiting `nextEvent()` cooperatively cancels only
   that exported Rust future. BoltFFI marks the future cancelled and stops
   polling it; the documentation does not equate this with cancelling
   application work.
   [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
2. Cancelling the agent run means cancelling its retained canonical
   `CancellationToken`. The token is cloneable and its current `cancel()` is
   idempotent (`crates/pi-ai/src/cancellation.rs:27`,
   `crates/pi-ai/src/cancellation.rs:28`,
   `crates/pi-ai/src/cancellation.rs:62`).

The repository locks Tokio 1.53.1 (`Cargo.lock:2412`). Its locally installed
`mpsc::Receiver::recv` documentation says cancellation before completion
receives no message
(`/home/vikash/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.53.1/src/sync/mpsc/bounded.rs:199`).
Consequently, cancelling one pending `nextEvent()` and polling again must
deliver the same next event.

That property does not prevent a stall if Swift never resumes pulling:

1. The bounded event channel reaches capacity 128
   (`crates/pi-agent-runtime-tokio/src/lib.rs:33`).
2. The actor waits in `sender.send(...).await`
   (`crates/pi-agent-runtime-tokio/src/lib.rs:838`).
3. Because `drive_run` is awaiting `dispatch_event`, it cannot return to the
   mailbox selection that processes steering, follow-up, mailbox cancellation,
   and shutdown (`crates/pi-agent-runtime-tokio/src/lib.rs:748`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:774`).
4. Sinks and completion for that event have not yet run
   (`crates/pi-agent-runtime-tokio/src/lib.rs:842`).

`TokioAgentRun::cancel()` therefore uses the retained token directly and never
waits for the mailbox. It requests cancellation but does not discard lifecycle
events. Callers wanting committed cancellation delivery call `cancel()` and
continue pulling until an `AgentEventEnvelope` contains
`AgentEvent::RunFinished { outcome: RunOutcome::Cancelled { .. } }`. Callers
abandoning observations call
`cancelAndOutcome()`, which also closes/discards the receiver and waits for
settlement (`crates/pi-agent-core/src/events.rs:69`).

Cancelling a Swift task that was awaiting `outcome()` also cancels only that
awaited Rust future under the documented cooperative rule; the cached
watch-style completion remains available to a later `outcome()` call.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]

Direct model calls follow the same separation. Cancelling a Swift task awaiting
`TokioAssistantStream.nextEvent()` cancels only that receive future under
BoltFFI's documented cooperative cancellation behavior; it does not cancel the
stored model-call token.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
The pending receive is the same cancellation-safe Tokio bounded-receiver
operation used by `TokioAgentRun`, so the next call must observe the event that
was not received by the cancelled call. `TokioAssistantStream.cancel()` cancels
the model token. Continuing the pull loop then preserves the canonical in-band
`AssistantEvent::Cancelled` lifecycle (`crates/pi-ai/src/runtime.rs:42`,
`crates/pi-ai/src/streaming.rs:534`).

Cancelling the Swift task awaiting initial `streamModel(request:)` also cancels
only that exported establishment future under BoltFFI's cooperative rule.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
BoltFFI's documented cancellation path performs cleanup through the generated
future's `free` operation.
[https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]
Section 4.8 therefore places a Rust drop guard in that future; the guard cancels
the stored token and closes observation when `free` releases the cancelled
future. That cleanup behavior is a property of the proposed canonical Rust
factory, not an additional BoltFFI behavior.

A direct model stream can stall for the same reason if Swift neither resumes
pulling nor abandons observations: its bounded Rust channel fills and the model
producer waits before it can poll the cancellation terminal. Call
`cancelAndWait()` when no more events are wanted; that operation cancels the
token, closes/discards observations, wakes a blocked sender, internally drains
to a validated terminal event, and waits for the producer's runtime lease to
settle. Cancelling the Swift task awaiting `cancelAndWait()` cancels only that
one exported future; its watch-style completion remains reusable under the same
design rule as Agent outcome.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]

## 7. Swift consumer shapes

These sketches show the ordinary generated-call shape after the canonical
changes. They remain explicitly illustrative inferences: exact generated Swift
declarations, field spellings, and enum-case pattern syntax must be confirmed by
generation. The inference composes the documented async, `Result`, `Option`,
class, async-trait, record-field, and associated-data-enum examples.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs-with-strings-or-collections]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]

Section 4.5 selects `AgentEventEnvelope`: every Agent `nextEvent()` below yields
that envelope, and sink callbacks receive that same value. Consumers inspect
`envelope.event` for lifecycle payloads while retaining `sequence` and `runId`
for gap detection and persistence. Those field and associated-data case
spellings are part of the same qualified inference, based on the documented
Swift struct-field and associated-data-enum examples; they are not claimed as
observed generated output for this repository.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs-with-strings-or-collections]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]

### Production construction

The first generated production path constructs a real configured canonical
control plane, then passes that object with canonical state and tools; Swift
never constructs a bare `Agent` or supplies provider/transport/auth traits:

```swift
func makeHandle(
    openAIApiKey: String,
    state: AgentState
) throws -> TokioAgentHandle {
    let modelsFactory = try OpenAiModelsFactory(
        apiKey: openAIApiKey
    )
    let models = try modelsFactory.build()
    let tools = ToolRegistry()
    let owner = try TokioRuntimeOwner(
        models: models,
        tools: tools
    )
    return try owner.spawnAgent(state: state)
}
```

This function is illustrative call-site code, not a required wrapper. The
generated surface is `OpenAiModelsFactory`, its `build` method, the
`TokioRuntimeOwner` constructor, and `spawnAgent`. BoltFFI documents fallible
constructors, class-valued methods, and constructors accepting borrowed
Rust-backed classes.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#fallible-constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
The value returned by `build` is the existing canonical `Models`, configured
with the current OpenAI provider/catalog/auth contracts and a concrete native
transport as specified in section 4.7; it is not a test fixture or
binding-specific record. The runtime lease captured by the actor makes it safe
for this local `owner` reference to leave scope after construction; section
4.7 requires `shutdown()` to await actor-task settlement.

### Pull observation

```swift
let run = try await handle.promptText(prompt: prompt)

while let envelope = try await run.nextEvent() {
    switch envelope.event {
    case .assistantUpdate(_, let update):
        render(update)

    case .runFinished(let outcome):
        display(outcome)

    default:
        break
    }
}

let outcome = try await run.outcome()
```

The final `outcome()` validates actor completion and all sink barriers.

### Cancellation with lifecycle delivery

```swift
run.cancel()

while let envelope = try await run.nextEvent() {
    consume(envelope)
}

let outcome = try await run.outcome()
```

### Cancellation without further observations

```swift
let outcome = try await run.cancelAndOutcome()
```

### Acknowledged sink

**Conditional surface.** This sketch is valid only if the annotation milestone
proves that BoltFFI generates an async host trait whose method accepts the owned
Rust-backed `CancellationToken`. That combination is
**UNRESOLVED: not answered by the documentation**; pages checked:
`callbacks.md#async-methods`,
`callbacks.md#ownership`, and
`classes.md#methods-that-take-or-return-classes`.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]

```swift
final class SessionSink: AgentEventSink {
    func onEvent(
        envelope: AgentEventEnvelope,
        cancellation: CancellationToken
    ) async {
        await session.append(envelope)
    }
}

let run = try await handle.promptTextWithSink(
    prompt: prompt,
    sink: SessionSink()
)

let outcome = try await run.outcome()
```

The sink receives the exact envelope allocated for pull delivery and
persistence. The sink-only actor path has no observational sender, so the
sketch needs no hidden drainer. Inside `onEvent`, use the supplied
`CancellationToken`, `cancelNow`, or `latestSnapshot`; do not await a mailbox
method that is queued behind the sink acknowledgement.

### Direct model-call observation

The direct R3 path uses the same while-let pull shape and the same canonical
`ModelRequest` and `AssistantEvent` values as Rust:

```swift
let stream = try await owner.streamModel(request: request)

while let event = try await stream.nextEvent() {
    consume(event)
}
```

`stream.cancel()` followed by continued pulling preserves delivery of
`.cancelled`; when no more events are wanted, use:

```swift
try await stream.cancelAndWait()
```

The exact Swift spellings are inferred by composing the documented async class,
throwing-result, and optional mappings.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#option]

## 8. Gaps and implementation risks

### 8.1 Authoritative-stream gaps are resolved by design

There is no documented lossless/backpressured overflow mode and no documented
direct adaptation from `futures_core::Stream` or Tokio receivers.
**UNRESOLVED: not answered by the documentation**; pages checked:
`streaming.md#the-ffi_stream-attribute`, `streaming.md#buffer-capacity`,
`streaming.md#stopping-streams`, and `experimental.md#feature-details`.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#stopping-streams]
[https://www.boltffi.dev/docs/experimental.md | docs/boltffi-swift-bindings/docs-snapshot/experimental.md#feature-details]
The design does not need such a mode: an ordinary async pull method preserves
the existing receiver.

### 8.2 Transitive owned-value graph

The in-scope request, event, snapshot, and control graph is not yet directly
annotatable in full:

- Open IDs such as `RunId`, `MessageId`, and `ToolCallId` are tuple newtypes
  generated by the macro at `crates/pi-ai/src/ids.rs:6`.
- `QueueSequence` and `EventSinkId` are tuple newtypes
  (`crates/pi-agent-core/src/control.rs:19`,
  `crates/pi-agent-runtime-tokio/src/lib.rs:35`).
- `ControlError::QueueFull.capacity` is `usize`
  (`crates/pi-agent-core/src/control.rs:68`,
  `crates/pi-agent-core/src/control.rs:74`). This is a separate transitive
  boundary type from the `QueueSequence` tuple newtype and from the error
  enum's `#[non_exhaustive]` marker (`crates/pi-agent-core/src/control.rs:66`).
- `Timestamp` is a non-ID `i64` tuple newtype and is reachable through every
  user/assistant/tool-result timestamp and through the active assistant
  snapshot (`crates/pi-ai/src/ids.rs:133`,
  `crates/pi-ai/src/messages.rs:123`,
  `crates/pi-ai/src/messages.rs:169`,
  `crates/pi-ai/src/messages.rs:211`,
  `crates/pi-ai/src/messages.rs:241`,
  `crates/pi-ai/src/streaming.rs:1703`).
- `Currency` is a non-ID `String` tuple newtype inside every `Cost`, including
  costs reached from `RunOutcome`, terminal `AssistantMessage`, and active
  `AssistantMessageSnapshot` (`crates/pi-ai/src/usage.rs:88`,
  `crates/pi-ai/src/usage.rs:121`,
  `crates/pi-agent-core/src/events.rs:59`,
  `crates/pi-ai/src/messages.rs:165`,
  `crates/pi-ai/src/streaming.rs:1701`).
- `ReplayDropReason` is a non-ID `String` tuple newtype reached through
  `AgentEvent::ContextPrepared.report -> HandoffReport.changes ->
  HandoffChange::OpaqueReplayDropped`
  (`crates/pi-ai/src/handoff.rs:44`,
  `crates/pi-agent-core/src/events.rs:97`,
  `crates/pi-agent-core/src/events.rs:103`,
  `crates/pi-ai/src/handoff.rs:84`,
  `crates/pi-ai/src/handoff.rs:92`,
  `crates/pi-ai/src/handoff.rs:150`).
- `AgentRecord::Custom` contains `Box<serde_json::value::RawValue>`
  (`crates/pi-agent-core/src/state.rs:66`,
  `crates/pi-agent-core/src/state.rs:70`).
- `AgentSnapshot::pending_tool_calls` is `Arc<[ToolCallId]>`
  (`crates/pi-agent-core/src/state.rs:190`).
- `AgentSnapshot::streaming` is an independent
  `Option<AssistantMessageSnapshot>` root, not a view of
  `AgentSnapshot::state.transcript`
  (`crates/pi-agent-core/src/state.rs:184`,
  `crates/pi-agent-core/src/state.rs:188`). Its active/partial value separately
  contains `DeferredHandle`, diagnostics, partial content, replay, usage,
  optional cost, `Timestamp`, and an optional terminal `AssistantMessage`
  (`crates/pi-ai/src/streaming.rs:1689`,
  `crates/pi-ai/src/streaming.rs:1693`,
  `crates/pi-ai/src/streaming.rs:1695`,
  `crates/pi-ai/src/streaming.rs:1697`,
  `crates/pi-ai/src/streaming.rs:1699`,
  `crates/pi-ai/src/streaming.rs:1701`,
  `crates/pi-ai/src/streaming.rs:1703`,
  `crates/pi-ai/src/streaming.rs:1705`). This root therefore reaches
  `DeferredHandle.data`, diagnostic `serde_json::Number` and
  `BTreeMap<String, serde_json::Value>`, partial
  `ContentBlock::ToolCall -> ToolCall.arguments`, `Currency`, and the complete
  terminal-message graph independently of any committed record
  (`crates/pi-ai/src/deferred.rs:39`,
  `crates/pi-ai/src/messages.rs:182`,
  `crates/pi-ai/src/messages.rs:217`,
  `crates/pi-ai/src/messages.rs:278`,
  `crates/pi-ai/src/messages.rs:306`,
  `crates/pi-ai/src/usage.rs:121`).
- `ReplayEnvelope` is not a scalar or opaque leaf. It contains
  `schema_version`, a full `ReplayScope`, and an ordered `Vec<ReplayItem>`
  (`crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:15`,
  `crates/pi-ai/src/replay.rs:17`, `crates/pi-ai/src/replay.rs:19`). Every item
  carries `id`, `ordinal`, `ReplayTarget`, `ReplayKind`,
  `ReplayApplicability`, `ReplayCompleteness`, and `OpaquePayload`
  (`crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:137`,
  `crates/pi-ai/src/replay.rs:139`, `crates/pi-ai/src/replay.rs:141`,
  `crates/pi-ai/src/replay.rs:143`, `crates/pi-ai/src/replay.rs:145`,
  `crates/pi-ai/src/replay.rs:147`, `crates/pi-ai/src/replay.rs:149`). The four
  targets are `Message`, `ContentBlock(ContentBlockId)`,
  `ToolCall(ToolCallId)`, and `ProviderOutputItem { output_index }`; the three
  payloads are `Utf8(String)`, `Bytes(Vec<u8>)`, and
  `JsonBytes(Vec<u8>)` (`crates/pi-ai/src/replay.rs:179`,
  `crates/pi-ai/src/replay.rs:183`, `crates/pi-ai/src/replay.rs:185`,
  `crates/pi-ai/src/replay.rs:187`, `crates/pi-ai/src/replay.rs:276`,
  `crates/pi-ai/src/replay.rs:278`, `crates/pi-ai/src/replay.rs:280`,
  `crates/pi-ai/src/replay.rs:282`). The request/event/state/snapshot graph has
  thirteen independent envelope roots:

  1. `ModelRequest.context.messages -> Message::Assistant ->
     AssistantMessage.replay` (`crates/pi-ai/src/runtime.rs:17`,
     `crates/pi-ai/src/messages.rs:473`, `crates/pi-ai/src/messages.rs:36`,
     `crates/pi-ai/src/messages.rs:157`).
  2. Direct `AssistantEvent::Finished.message.replay`
     (`crates/pi-ai/src/streaming.rs:521`,
     `crates/pi-ai/src/messages.rs:157`).
  3. Direct `AssistantEvent::Failed.message.replay`
     (`crates/pi-ai/src/streaming.rs:528`,
     `crates/pi-ai/src/messages.rs:157`).
  4. Direct `AssistantEvent::Cancelled.message.replay`
     (`crates/pi-ai/src/streaming.rs:534`,
     `crates/pi-ai/src/messages.rs:157`).
  5. `AgentEvent::AssistantUpdate` carrying `Finished`
     (`crates/pi-agent-core/src/events.rs:113`,
     `crates/pi-ai/src/streaming.rs:521`,
     `crates/pi-ai/src/messages.rs:157`).
  6. `AgentEvent::AssistantUpdate` carrying `Failed`
     (`crates/pi-agent-core/src/events.rs:113`,
     `crates/pi-ai/src/streaming.rs:528`,
     `crates/pi-ai/src/messages.rs:157`).
  7. `AgentEvent::AssistantUpdate` carrying `Cancelled`
     (`crates/pi-agent-core/src/events.rs:113`,
     `crates/pi-ai/src/streaming.rs:534`,
     `crates/pi-ai/src/messages.rs:157`).
  8. `AgentEvent::MessageCommitted` carrying an assistant `AgentRecord`
     (`crates/pi-agent-core/src/events.rs:120`,
     `crates/pi-agent-core/src/state.rs:64`,
     `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
  9. A standalone assistant `AgentRecord`
     (`crates/pi-agent-core/src/state.rs:64`,
     `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
  10. A direct `AgentState.transcript` assistant record
      (`crates/pi-agent-core/src/state.rs:33`,
      `crates/pi-agent-core/src/state.rs:64`,
      `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
  11. `AgentSnapshot.state.transcript` containing an assistant record
      (`crates/pi-agent-core/src/state.rs:184`,
      `crates/pi-agent-core/src/state.rs:33`,
      `crates/pi-agent-core/src/state.rs:64`,
      `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
  12. `AgentSnapshot.streaming.replay`
      (`crates/pi-agent-core/src/state.rs:188`,
      `crates/pi-ai/src/streaming.rs:1697`).
  13. `AgentSnapshot.streaming.terminal_message.replay`
      (`crates/pi-agent-core/src/state.rs:188`,
      `crates/pi-ai/src/streaming.rs:1705`,
      `crates/pi-ai/src/messages.rs:157`).

  Each root must be generated and round-tripped with its own distinct,
  nonempty envelope. Every envelope uses the exact canonical `schema_version`
  and root-specific values for all five `ReplayScope` fields, item count/order,
  every item
  ID/ordinal/kind/applicability/completeness, target IDs or indexes, and payload
  strings or bytes. Each envelope independently exercises all four
  `ReplayTarget` forms and all three `OpaquePayload` forms. An empty envelope,
  a reused sentinel set, or a successful path through any other root is not
  evidence for that root.
- Replay also has direct event roots before terminal assembly:
  `AssistantEvent::ReplayItemStarted` carries `ReplayTarget`, and
  `AssistantEvent::ReplayData` carries `ReplayDataOperation`
  (`crates/pi-ai/src/streaming.rs:474`,
  `crates/pi-ai/src/streaming.rs:480`,
  `crates/pi-ai/src/streaming.rs:488`,
  `crates/pi-ai/src/streaming.rs:492`). `ReplayDataOperation` has the five
  tuple-style forms `ReplaceUtf8`, `AppendUtf8`, `ReplaceBytes`,
  `AppendBytes`, and `ReplaceJsonBytes`
  (`crates/pi-ai/src/streaming.rs:563`,
  `crates/pi-ai/src/streaming.rs:565`,
  `crates/pi-ai/src/streaming.rs:567`,
  `crates/pi-ai/src/streaming.rs:569`,
  `crates/pi-ai/src/streaming.rs:571`,
  `crates/pi-ai/src/streaming.rs:573`). Each target form and operation form
  must be proven once as a standalone `AssistantEvent` and again nested in
  `AgentEvent::AssistantUpdate` (`crates/pi-agent-core/src/events.rs:113`,
  `crates/pi-agent-core/src/events.rs:117`).
- `AgentEvent::ContextPrepared.report` reaches
  `HandoffReport::source_models: BTreeSet<ModelFingerprint>`
  (`crates/pi-agent-core/src/events.rs:97`,
  `crates/pi-agent-core/src/events.rs:103`,
  `crates/pi-ai/src/handoff.rs:144`,
  `crates/pi-ai/src/handoff.rs:146`).
- `RunOutcome` reaches `Cost`, whose `currency` is the `Currency` tuple newtype
  and whose `micros` is `i128`
  (`crates/pi-agent-core/src/events.rs:59`,
  `crates/pi-ai/src/usage.rs:119`,
  `crates/pi-ai/src/usage.rs:121`,
  `crates/pi-ai/src/usage.rs:123`).
- Direct `ModelRequest::options` reaches
  `SimpleGenerationOptions::sampling: OrderedJsonObject`. That type is itself
  a tuple newtype over `IndexMap<OrderedJsonString, OrderedJsonValue>`; the
  recursive value graph contains the tuple newtypes `OrderedJsonString` and
  `OrderedJsonArray`, and arrays recursively contain `OrderedJsonValue`
  (`crates/pi-ai/src/runtime.rs:19`,
  `crates/pi-ai/src/options.rs:598`,
  `crates/pi-ai/src/json_compat.rs:24`,
  `crates/pi-ai/src/json_compat.rs:114`,
  `crates/pi-ai/src/json_compat.rs:227`,
  `crates/pi-ai/src/json_compat.rs:324`,
  `crates/pi-ai/src/json_compat.rs:335`,
  `crates/pi-ai/src/json_compat.rs:337`,
  `crates/pi-ai/src/json_compat.rs:339`).
- `SimpleGenerationOptions::headers` is the canonical `HeaderMapSpec` alias for
  `BTreeMap<String, Option<String>>`
  (`crates/pi-ai/src/options.rs:602`,
  `crates/pi-ai/src/model.rs:17`).
- `SimpleGenerationOptions::api_options` reaches
  `ErasedApiOptionsPatch::value: Box<RawValue>`
  (`crates/pi-ai/src/options.rs:611`,
  `crates/pi-ai/src/options.rs:294`,
  `crates/pi-ai/src/options.rs:300`).
- `ToolCall.arguments` is `serde_json::Value`
  (`crates/pi-ai/src/messages.rs:300`,
  `crates/pi-ai/src/messages.rs:306`). It reaches every terminal
  `AssistantMessage` that contains a tool-call content block and therefore
  `AssistantEvent::{Finished, Failed, Cancelled}`
  (`crates/pi-ai/src/messages.rs:155`,
  `crates/pi-ai/src/messages.rs:278`,
  `crates/pi-ai/src/messages.rs:282`,
  `crates/pi-ai/src/streaming.rs:521`,
  `crates/pi-ai/src/streaming.rs:527`,
  `crates/pi-ai/src/streaming.rs:533`). The same value independently reaches
  `AgentEvent::ToolExecutionStarted`
  (`crates/pi-agent-core/src/events.rs:125`,
  `crates/pi-agent-core/src/events.rs:127`), assistant
  `AgentEvent::MessageCommitted` values, `AgentState::transcript`, and
  `AgentSnapshot::state`
  (`crates/pi-agent-core/src/events.rs:120`,
  `crates/pi-agent-core/src/events.rs:122`,
  `crates/pi-agent-core/src/state.rs:33`,
  `crates/pi-agent-core/src/state.rs:184`). It is also a direct-request path
  through `ModelRequest.context -> Context.messages`
  (`crates/pi-ai/src/runtime.rs:17`,
  `crates/pi-ai/src/messages.rs:473`). Independently, the same arguments can
  be present in active partial content at
  `AgentSnapshot.streaming -> AssistantMessageSnapshot.content`
  (`crates/pi-agent-core/src/state.rs:188`,
  `crates/pi-ai/src/streaming.rs:1695`,
  `crates/pi-ai/src/messages.rs:278`,
  `crates/pi-ai/src/messages.rs:306`) and again inside that snapshot's optional
  `terminal_message` (`crates/pi-ai/src/streaming.rs:1705`).
- `ToolSpec.parameters` is another `serde_json::Value` root, and reaches direct
  `ModelRequest.context` through `Context.tools`
  (`crates/pi-ai/src/messages.rs:311`,
  `crates/pi-ai/src/messages.rs:319`,
  `crates/pi-ai/src/messages.rs:475`,
  `crates/pi-ai/src/runtime.rs:17`).
- `DeferredHandle.data` is `Option<serde_json::Value>` and reaches all three
  terminal assistant events through `AssistantMessage.deferred`
  (`crates/pi-ai/src/deferred.rs:39`,
  `crates/pi-ai/src/messages.rs:143`,
  `crates/pi-ai/src/streaming.rs:521`,
  `crates/pi-ai/src/streaming.rs:527`,
  `crates/pi-ai/src/streaming.rs:533`). A committed terminal assistant also
  carries it into `AgentEvent::MessageCommitted`, the transcript, and snapshots
  through the record paths cited above; `Message::Assistant` in
  `Context.messages` carries the same value into a later direct
  `ModelRequest.context`
  (`crates/pi-ai/src/messages.rs:36`,
  `crates/pi-ai/src/messages.rs:473`,
  `crates/pi-ai/src/runtime.rs:17`). Independently, an active
  `AgentSnapshot.streaming` carries the handle directly in
  `AssistantMessageSnapshot.deferred` and may carry it again in
  `terminal_message`
  (`crates/pi-agent-core/src/state.rs:188`,
  `crates/pi-ai/src/streaming.rs:1689`,
  `crates/pi-ai/src/streaming.rs:1705`).
- `ToolUpdate.details` and `ToolOutput.details` are each
  `Option<Box<RawValue>>`; they reach, respectively,
  `AgentEvent::ToolExecutionUpdated` and
  `AgentEvent::ToolExecutionFinished`
  (`crates/pi-agent-core/src/tools.rs:89`,
  `crates/pi-agent-core/src/tools.rs:93`,
  `crates/pi-agent-core/src/tools.rs:47`,
  `crates/pi-agent-core/src/tools.rs:51`,
  `crates/pi-agent-core/src/events.rs:130`,
  `crates/pi-agent-core/src/events.rs:134`,
  `crates/pi-agent-core/src/events.rs:137`,
  `crates/pi-agent-core/src/events.rs:141`).
- `ToolResultMessage.details` reaches
  `VersionedExtension.value: Box<RawValue>`
  (`crates/pi-ai/src/messages.rs:222`,
  `crates/pi-ai/src/messages.rs:233`,
  `crates/pi-ai/src/model.rs:919`,
  `crates/pi-ai/src/model.rs:923`). The Agent constructs that extension from
  `ToolOutput.details`, commits the `ToolResultMessage` as an `AgentRecord`, and
  retains it in `AgentState::transcript` and `AgentSnapshot::state`
  (`crates/pi-agent-core/src/run.rs:1673`,
  `crates/pi-agent-core/src/run.rs:1682`,
  `crates/pi-agent-core/src/run.rs:1686`,
  `crates/pi-agent-core/src/run.rs:1278`,
  `crates/pi-agent-core/src/run.rs:1282`,
  `crates/pi-agent-core/src/state.rs:33`,
  `crates/pi-agent-core/src/state.rs:184`). `Message::ToolResult` in
  `Context.messages` also makes the same extension part of the complete direct
  `ModelRequest.context` value graph
  (`crates/pi-ai/src/messages.rs:38`,
  `crates/pi-ai/src/messages.rs:473`,
  `crates/pi-ai/src/runtime.rs:17`).
- `GrammarVariants` is `BTreeMap<GrammarFormat, String>`. It is a third
  independent `BTreeMap` path, beyond headers and diagnostic details, and
  reaches a direct request through
  `ToolSpec.constrained_sampling -> ConstrainedSamplingConfig::Grammar ->
  Context.tools -> ModelRequest.context`
  (`crates/pi-ai/src/messages.rs:325`,
  `crates/pi-ai/src/messages.rs:334`,
  `crates/pi-ai/src/messages.rs:379`,
  `crates/pi-ai/src/messages.rs:387`,
  `crates/pi-ai/src/messages.rs:389`,
  `crates/pi-ai/src/messages.rs:418`,
  `crates/pi-ai/src/messages.rs:475`,
  `crates/pi-ai/src/runtime.rs:17`).
- `AssistantEvent::DiagnosticAdded` directly carries
  `AssistantMessageDiagnostic`; that record reaches
  `DiagnosticErrorCode::Number(serde_json::Number)` and
  `details: BTreeMap<String, serde_json::Value>`
  (`crates/pi-ai/src/streaming.rs:426`,
  `crates/pi-ai/src/streaming.rs:430`,
  `crates/pi-ai/src/messages.rs:176`,
  `crates/pi-ai/src/messages.rs:182`,
  `crates/pi-ai/src/messages.rs:204`,
  `crates/pi-ai/src/messages.rs:217`). The same two types are also reachable
  through the `AssistantMessage::diagnostics` field carried by
  `AssistantEvent::{Finished, Failed, Cancelled}`
  (`crates/pi-ai/src/messages.rs:150`,
  `crates/pi-ai/src/messages.rs:152`,
  `crates/pi-ai/src/streaming.rs:521`,
  `crates/pi-ai/src/streaming.rs:527`,
  `crates/pi-ai/src/streaming.rs:533`). Committed assistants retain diagnostics
  in records and snapshots, and `Message::Assistant` in `Context.messages`
  carries them into later direct requests
  (`crates/pi-ai/src/messages.rs:36`,
  `crates/pi-ai/src/messages.rs:473`,
  `crates/pi-agent-core/src/state.rs:33`,
  `crates/pi-agent-core/src/state.rs:184`,
  `crates/pi-ai/src/runtime.rs:17`). Independently, active snapshots carry the
  diagnostic vector directly in `AssistantMessageSnapshot.diagnostics` and
  may carry the same graph again through `terminal_message`
  (`crates/pi-agent-core/src/state.rs:188`,
  `crates/pi-ai/src/streaming.rs:1693`,
  `crates/pi-ai/src/streaming.rs:1705`).
- Eight canonical data enums in this graph use tuple-style variants:
  `Message`, `AgentRecord`, `DiagnosticErrorCode`, `ConstrainedSampling`,
  `OrderedJsonValue`, `ReplayTarget`, `OpaquePayload`, and
  `ReplayDataOperation` (`crates/pi-ai/src/messages.rs:32`,
  `crates/pi-agent-core/src/state.rs:62`,
  `crates/pi-ai/src/messages.rs:178`,
  `crates/pi-ai/src/messages.rs:334`,
  `crates/pi-ai/src/json_compat.rs:324`,
  `crates/pi-ai/src/replay.rs:179`,
  `crates/pi-ai/src/replay.rs:276`,
  `crates/pi-ai/src/streaming.rs:563`). They are a distinct syntax gate from
  tuple-newtype records and from tuple-payload error variants.

The numeric quick-reference table lists `i8` through `i64` and `u8` through
`u64`, but does not list `usize`. The primitives section contains an isolated
`usize` function-argument example; it does not state whether `usize` is
supported as a field of an error variant. For
`ControlError::QueueFull.capacity`, **UNRESOLVED: not answered by the
documentation**. Pages checked: `types.md#quick-reference`,
`types.md#primitives`, `errors.md#enum-errors`, and
`errors.md#enums-with-payloads`.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

Tuple-newtype generation, `RawValue`, `Arc<[T]>`, `IndexMap`, `BTreeMap`,
`BTreeSet`, `serde_json::Number`, `serde_json::Value`, and direct
`i128`/`u128` mapping are
**UNRESOLVED: not answered by the documentation**. Pages checked:
`records.md#structs`, `types.md#records`, `types.md#quick-reference`,
`types.md#collections`, `types.md#nested-collections`,
`types.md#built-in-custom-types`, `types.md#whats-not-supported`, and
`custom-types.md#representation-types`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#collections]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#nested-collections]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#built-in-custom-types]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]

Tuple-style data-enum variant generation is also
**UNRESOLVED: not answered by the documentation**. The records page shows unit
variants and struct-style associated-data variants, but does not show tuple
variants. Pages checked: `records.md#enums`,
`records.md#enums-with-associated-data`, and `types.md#records`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
The unresolved result applies separately to `Message`, `AgentRecord`,
`DiagnosticErrorCode`, `ConstrainedSampling`, `OrderedJsonValue`,
`ReplayTarget`, `OpaquePayload`, and `ReplayDataOperation`. Generation and an
exact Swift round trip are required for every variant of every enum; success
for a shared payload type or for one tuple-style enum cannot be generalized to
another. Failure returns the implementation to the canonical inline Rust type
design; it must not produce a binding-only enum or envelope.

The tuple-newtype unresolved result applies independently to the macro IDs,
`QueueSequence`, `EventSinkId`, `Timestamp`, `Currency`,
`ReplayDropReason`, `OrderedJsonString`, `OrderedJsonObject`, and
`OrderedJsonArray`; the design does not treat a successful ID probe as evidence
for any of the other wrappers. The records page demonstrates named-field
structs but does not establish tuple-newtype generation.
**UNRESOLVED: not answered by the documentation**. Pages checked:
`records.md#structs`, `types.md#records`, and
`custom-types.md#representation-types`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]

These are hard generation gates, not omissions to be patched with lossy
strings. Until a direct mapping or lossless inline canonical conversion is
proven, do not annotate the whole affected enum/value graph:

- `BTreeSet<ModelFingerprint>` gates `AgentEvent::ContextPrepared`, and because
  `AgentEvent` is annotated as one payload enum, gates the exported
  `AgentEvent`/`AgentEventEnvelope` pull and sink surface. The same event case
  separately reaches `ReplayDropReason` through `HandoffReport.changes`, so
  resolving `BTreeSet` alone does not resolve `ContextPrepared`
  (`crates/pi-agent-core/src/events.rs:97`,
  `crates/pi-agent-core/src/events.rs:103`,
  `crates/pi-ai/src/handoff.rs:92`,
  `crates/pi-ai/src/handoff.rs:146`,
  `crates/pi-ai/src/handoff.rs:150`).
- `ControlError::QueueFull.capacity` has its own generated error-payload gate.
  A generation and Swift catch/payload-fidelity test must preserve exact `usize`
  values; success for `QueueSequence`, another numeric payload, or
  `ControlError`'s `#[non_exhaustive]` switch shape is not evidence for this
  field (`crates/pi-agent-core/src/control.rs:68`,
  `crates/pi-agent-core/src/control.rs:74`). The documentation does not state
  support for `usize` error payloads. **UNRESOLVED: not answered by the
  documentation**. Pages checked: `types.md#quick-reference`,
  `types.md#primitives`, and `errors.md#enums-with-payloads`.
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
  [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
- `serde_json::Value` must be proven on four independent canonical roots.
  `ToolCall.arguments` gates terminal `AssistantEvent` values,
  `AgentEvent::ToolExecutionStarted`, assistant committed records and
  `AgentSnapshot.state`, the separate active
  `AgentSnapshot.streaming.content` and `.terminal_message` paths, and direct
  `ModelRequest.context`; `ToolSpec.parameters` gates
  direct requests through `Context.tools`; `DeferredHandle.data` gates all
  terminal `AssistantEvent` values, the terminal messages later committed into
  records and `AgentSnapshot.state`, the separate
  `AgentSnapshot.streaming.deferred` and `.terminal_message` paths, and later
  direct requests containing those messages.
  `AssistantMessageDiagnostic.details` is a fourth use inside
  `DiagnosticAdded`, terminal messages, committed records/snapshots, and later
  direct requests, and independently inside
  `AgentSnapshot.streaming.diagnostics` and `.terminal_message`. None of the
  Agent pull, sink, direct assistant pull, record/snapshot, active snapshot, or
  direct request roots advances until its reachable `Value` fields generate
  and preserve the full values.
- `RawValue` must be proven on every ownership/container path:
  `AgentRecord::Custom.payload`, `ErasedApiOptionsPatch::value`, transient
  `ToolUpdate.details`/`ToolOutput.details` in
  `ToolExecutionUpdated`/`ToolExecutionFinished`, and durable
  `ToolResultMessage.details -> VersionedExtension.value` in committed records
  and snapshots and in later direct requests carrying those tool-result
  messages. A successful probe of one `Box<RawValue>` occurrence does not by
  itself validate every enclosing event, option, record, snapshot, and request
  root.
- `BTreeMap` must be proven separately for request headers
  (`BTreeMap<String, Option<String>>`), diagnostic details
  (`BTreeMap<String, serde_json::Value>`), and `GrammarVariants`
  (`BTreeMap<GrammarFormat, String>`) inside `ToolSpec` on the direct-request
  path. Diagnostic details also occur in committed records/snapshots, in the
  independent `AgentSnapshot.streaming.diagnostics` and `.terminal_message`
  paths, and in later direct requests. Different key/value graphs and nesting
  mean that resolving only one use leaves the other request/event/record/
  snapshot roots gated.
- `serde_json::Number` gates `AssistantEvent::DiagnosticAdded` and the three
  terminal variants through their complete `AssistantMessage`, and therefore
  also gates nested `AgentEvent::AssistantUpdate`, committed assistant records,
  `AgentSnapshot.state`, the independent
  `AgentSnapshot.streaming.diagnostics` and `.terminal_message` paths, and
  later direct requests containing those messages.
- Replay fidelity gates thirteen independent `ReplayEnvelope` roots, each of
  which must be nonempty: (1) a direct `ModelRequest` assistant message; (2)
  direct `AssistantEvent::Finished`; (3) direct `AssistantEvent::Failed`; (4)
  direct `AssistantEvent::Cancelled`; (5) `AgentEvent::AssistantUpdate` carrying
  `Finished`; (6) `AgentEvent::AssistantUpdate` carrying `Failed`; (7)
  `AgentEvent::AssistantUpdate` carrying `Cancelled`; (8) assistant
  `AgentEvent::MessageCommitted`; (9) a standalone assistant `AgentRecord`;
  (10) direct `AgentState.transcript`; (11) `AgentSnapshot.state.transcript`;
  (12) `AgentSnapshot.streaming.replay`; and (13)
  `AgentSnapshot.streaming.terminal_message.replay`
  (`crates/pi-ai/src/runtime.rs:17`, `crates/pi-ai/src/messages.rs:36`,
  `crates/pi-ai/src/messages.rs:157`, `crates/pi-ai/src/messages.rs:473`,
  `crates/pi-ai/src/streaming.rs:521`, `crates/pi-ai/src/streaming.rs:528`,
  `crates/pi-ai/src/streaming.rs:534`,
  `crates/pi-agent-core/src/events.rs:113`,
  `crates/pi-agent-core/src/events.rs:120`,
  `crates/pi-agent-core/src/state.rs:33`,
  `crates/pi-agent-core/src/state.rs:64`,
  `crates/pi-agent-core/src/state.rs:184`,
  `crates/pi-agent-core/src/state.rs:188`,
  `crates/pi-ai/src/streaming.rs:1697`,
  `crates/pi-ai/src/streaming.rs:1705`). Each envelope must preserve the exact
  canonical schema version, a distinct root-specific scope/item sentinel set,
  ordered nonempty items, every item field, all four `ReplayTarget` forms, and
  all three `OpaquePayload` forms
  (`crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:88`,
  `crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:179`,
  `crates/pi-ai/src/replay.rs:276`). The standalone
  `AssistantEvent::{ReplayItemStarted,ReplayData}` matrix and its corresponding
  nested `AgentEvent::AssistantUpdate` matrix are additional direct-event gates;
  together they must preserve all four targets and all five
  `ReplayDataOperation` forms with distinct direct/nested sentinels
  (`crates/pi-ai/src/streaming.rs:474`,
  `crates/pi-ai/src/streaming.rs:488`,
  `crates/pi-ai/src/streaming.rs:563`,
  `crates/pi-agent-core/src/events.rs:113`). An empty replay vector, a shared
  sentinel set, or one successful path cannot discharge another root.
- Tuple-style data-enum syntax independently gates every variant of `Message`,
  `AgentRecord`, `DiagnosticErrorCode`, `ConstrainedSampling`,
  `OrderedJsonValue`, `ReplayTarget`, `OpaquePayload`, and
  `ReplayDataOperation`. It is not discharged by the separate tuple-newtype
  or tuple-payload error probes. The records documentation does not establish
  tuple-variant syntax. **UNRESOLVED: not answered by the documentation**.
  Pages checked: `records.md#enums`,
  `records.md#enums-with-associated-data`, and `types.md#records`.
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
  [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
  [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
- Every tuple wrapper is a separate fidelity gate: IDs and queue/sink
  sequences on their event/control roots; `Timestamp` on messages,
  diagnostics, terminal events, and active snapshots; `Currency` on every
  `Cost` in outcomes, terminal messages, and active snapshots;
  `ReplayDropReason` on `ContextPrepared`; and `OrderedJsonString`,
  `OrderedJsonObject`, and `OrderedJsonArray` on the recursively nested
  sampling graph. `Arc<[ToolCallId]>`, the `IndexMap` storage beneath ordered
  JSON, and `i128` cost remain separate gates. Resolving JSON/container paths
  or a tuple-ID probe does not resolve any of these types.

The documented custom-type mechanisms can convert a whole owner-defined type to
a supported representation, but they require conversion code and conversion
failure panics.
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#choosing-an-approach]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors]
Any use must remain an inline conversion for the same canonical type and must
preserve its full value exactly; it may not introduce a parallel Swift-only
record hierarchy. Naked 128-bit method parameters or returns remain unresolved
even if a whole record receives a custom conversion.

### 8.3 `TokioAgentError::Agent` payload shape

Current `TokioAgentError` contains `Agent(AgentError)`
(`crates/pi-agent-runtime-tokio/src/lib.rs:76`). That one variant raises two
separate mapping questions:

1. It is a tuple-payload error variant rather than a unit or struct-style
   variant. The documented error examples show unit variants and struct-style
   payload variants, but do not show a tuple-payload error variant.
   **UNRESOLVED: not answered by the documentation**. Pages checked:
   `errors.md#supported-error-types`, `errors.md#enum-errors`, and
   `errors.md#enums-with-payloads`.
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
2. Its payload type is itself the canonical `AgentError` error enum. The
   documented payload-error fields are `String` and `u32`; the page does not say
   whether an error enum may contain another error type as a payload.
   **UNRESOLVED: not answered by the documentation**. Pages checked:
   `errors.md#supported-error-types`, `errors.md#enum-errors`, and
   `errors.md#enums-with-payloads`.
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

Therefore annotation of `TokioAgentError`, and consequently every exported
method returning `Result<_, TokioAgentError>`, is gated on a minimal generation
and Swift throwing/catching test for both shapes. If either fails, implementation
must return to the canonical Rust error design; it must not flatten
`AgentError` into a binding-only string or parallel error envelope. The
documentation does establish that supported async `Result` errors become native
async errors, but that rule applies only after the error type itself is
supported.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#async-errors]

### 8.4 Non-exhaustive enums

All six current in-scope occurrences are gates:

1. `AgentEvent` (`crates/pi-agent-core/src/events.rs:78`).
2. `AssistantEvent` (`crates/pi-ai/src/streaming.rs:357`).
3. `RequestStartErrorKind` (`crates/pi-ai/src/runtime.rs:23`), which is the
   `kind` field of directly in-scope `RequestStartError`
   (`crates/pi-ai/src/runtime.rs:48`, `crates/pi-ai/src/runtime.rs:50`).
4. `TokioAgentError` (`crates/pi-agent-runtime-tokio/src/lib.rs:68`).
5. `AgentError` (`crates/pi-agent-core/src/error.rs:12`).
6. `ControlError` (`crates/pi-agent-core/src/control.rs:66`).

Target handling of `#[non_exhaustive]` data enums, error enums, or an error
record containing a non-exhaustive enum is
**UNRESOLVED: not answered by the documentation**. Pages checked:
`records.md#enums`, `records.md#enums-with-associated-data`,
`errors.md#enum-errors`, and `errors.md#enums-with-payloads`.
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
[https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

Do not silently remove any open-world marker to satisfy generation. Milestone
2 must generate each occurrence separately and compile a Swift switch/catch
smoke test for it before enabling the dependent surface. Failure for
`AgentEvent` blocks Agent pulls/sinks; failure for `AssistantEvent` blocks both
direct and nested assistant delivery; failure for `RequestStartErrorKind`
blocks `TokioRuntimeOwner::stream_model`; failure for the three current error
enums blocks every method returning the corresponding error. A successful test
for one occurrence is not evidence for another.
`ControlError` also remains independently gated on exact generation of its
`QueueFull.capacity: usize` payload; passing the non-exhaustive switch/catch
probe alone is insufficient (`crates/pi-agent-core/src/control.rs:68`,
`crates/pi-agent-core/src/control.rs:74`). The documentation does not establish
`usize` error payloads. **UNRESOLVED: not answered by the documentation**.
Pages checked: `types.md#quick-reference`, `types.md#primitives`, and
`errors.md#enums-with-payloads`.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

### 8.5 Feature gating and multi-crate discovery

The intended integration uses optional inline attributes so ordinary Rust builds
do not require binding generation. The documentation shows direct attributes
after `use boltffi::*`, but does not state whether discovery supports
`cfg_attr`, optional build dependencies, or annotated types from dependency
crates when one `source_crate` generates the package.
**UNRESOLVED: not answered by the documentation**; pages checked:
`installation.md#add-to-your-project`, `installation.md#create-buildrs`,
`getting-started.md#write-your-code`, and
`configuration.md#package-identity`.
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
[https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
Milestone 2 must prove this with the actual `pi-ai`, `pi-ai-openai`,
`pi-agent-core`, and `pi-agent-runtime-tokio` dependency graph before broad
annotation.

### 8.6 Callback argument direction

As flagged in section 1, the documented callback examples do not establish an
async protocol method with an owned Rust-backed class argument. If
`CancellationToken` fails the smoke test, the gap must be reported to the owner.
The design does not substitute an integer token, global cancellation command,
or unacknowledged stream callback.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]

### 8.7 Production construction is specified, not delegated to a fixture

Production construction is the complete chain below, not only the final actor
step:

```text
OpenAiModelsFactory::new(api_key)
        |
        | owns NativeOpenAiHttpTransport + canonical credential
        v
OpenAiModelsFactory::build() -> Models
        |
        v
TokioRuntimeOwner::new(&models, &tools)
        |
        v
TokioRuntimeOwner::spawn_agent(state) -> TokioAgentHandle
```

The first two operations are the concrete provider/transport/auth path from
section 4.7. They close the current gap in which `Models::default` builds an
empty provider list (`crates/pi-ai/src/models.rs:99`,
`crates/pi-ai/src/models.rs:101`,
`crates/pi-ai/src/models.rs:1460`), while existing registration requires
`ProviderRegistration` (`crates/pi-ai/src/models.rs:399`) and its trait-object
fields (`crates/pi-ai/src/provider.rs:2320`,
`crates/pi-ai/src/provider.rs:2324`,
`crates/pi-ai/src/provider.rs:2326`,
`crates/pi-ai/src/provider.rs:2331`). Arbitrary `dyn Trait` is not a documented
ordinary value mapping, so none is placed in the generated signature.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported]

The last two operations construct
`Agent::new(Arc::new(models.clone()), state, tools.clone())` inside the existing
Tokio crate and return `TokioAgentHandle`; the current `Agent::new` contract is
at `crates/pi-agent-core/src/run.rs:140`, and the concrete `Models` runtime impl
is at `crates/pi-ai/src/models.rs:1399`. The chain never accepts a foreign
`Agent`, the `ModelRuntime` trait-object seam, JSON commands, or a duplicate
record hierarchy.

`OpenAiModelsFactory`, `TokioRuntimeOwner`, and their constructors do not yet
exist, so they are implementation dependencies, not unresolved API choices.
The planned generated signatures use Rust-backed class construction,
class-valued return, and borrowed class arguments, all documented class forms.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
Provider implementation traits remain unannotated. The scripted and malformed
factories in section 9 remain test-only and cannot satisfy acceptance test 17,
which must invoke this production chain through generated Swift.

### 8.8 Coexistence with the current UniFFI binding

The existing `pi-ffi` crate is a separate UniFFI static/dynamic library
(`bindings/pi-ffi/Cargo.toml:1`) and checks in generated Swift
(`bindings/pi-ffi/generated/swift/PiFFI.swift:1`). No snapshot page describes
BoltFFI/UniFFI coexistence or migration.
**UNRESOLVED: not answered by the documentation**; pages checked:
`packaging.md#apple-packaging`,
`configuration.md#swift-module-name`, and
`configuration.md#swiftpm-layouts`.
[https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#apple-packaging]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swift-module-name]
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#swiftpm-layouts]
Keep it unchanged until the new package passes all acceptance tests; use a
distinct Swift module and native artifact name during evaluation.

## 9. Acceptance-test plan

Rust contract tests belong in
`crates/pi-agent-runtime-tokio/tests/boltffi_run_contract.rs` and
`crates/pi-agent-runtime-tokio/tests/boltffi_assistant_contract.rs`; tests that
must reach private actor machinery live in a `#[cfg(test)]` unit module beside
that machinery in `crates/pi-agent-runtime-tokio/src/lib.rs`. Envelope tests
also extend `crates/pi-agent-core/tests/m2_1_state.rs`. Generated-language tests
belong in a test-only SwiftPM package at
`bindings/boltffi-swift-tests/Tests/AgentPrismBoltFFIAcceptanceTests/`.
The Rust side of generated-language-only construction lives in the planned
`crates/pi-agent-runtime-tokio/src/boltffi_test_fixtures.rs`, compiled only when
both the binding and `boltffi-test-fixtures` test features are enabled. That
module is excluded from production generation. The SwiftPM package contains
XCTest code only—no production wrapper.

| # | Required proof | Where and how |
|---:|---|---|
| 1 | More than `DEFAULT_EVENT_CAPACITY` events with a deliberately slow Swift consumer; no loss or reordering | Rust test `pull_more_than_capacity_is_lossless` scripts at least `DEFAULT_EVENT_CAPACITY * 3` uniquely indexed updates and delays pulls; Swift `testSlowPullHasNoLossOrReordering` repeats through generated `nextEvent()` and compares the complete index vector. |
| 2 | `RunFinished` is always delivered | Rust `pull_delivers_run_finished_last` asserts exactly one envelope containing the terminal event and then validated EOF; Swift `testRunFinishedIsDelivered` asserts the same before `outcome()`. Reuse the existing lifecycle basis in `crates/pi-agent-core/tests/m2_2_run.rs:215`. |
| 3 | Core EOF without `RunFinished` throws `MissingRunFinished`, never `nil` | Do **not** use `ScriptedRuntime`: when a model stream lacks its terminal event, Agent core constructs/selects a failed assistant (`crates/pi-agent-core/src/run.rs:1126`, `crates/pi-agent-core/src/run.rs:1132`), commits it to the transcript (`crates/pi-agent-core/src/run.rs:1156`), and later emits `RunFinished` (`crates/pi-agent-core/src/run.rs:1187`). Put `drive_run_eof_without_run_finished_is_error` in the `#[cfg(test)]` unit module beside private `drive_run` (`crates/pi-agent-runtime-tokio/src/lib.rs:734`); feed a crate-private `SendBoxStream<AgentEvent>` containing `RunStarted` and raw EOF, then assert with `matches!` that `DriveResult.outcome` is `Err(TokioAgentError::MissingRunFinished)`. Put a separate `next_event_eof_replays_cached_missing_run_finished` unit there to construct the reshaped private fields and verify the public envelope pull never returns `Ok(None)`. For Swift, enable a `boltffi-test-fixtures`-only exported `malformed_agent_run_fixture()` in a test-support module; it constructs a `TokioAgentRun` whose envelope channel closes after a nonterminal event and whose cached completion is `MissingRunFinished`. `testMalformedEofThrowsMissingRunFinished` calls only that generated public fixture and `nextEvent()`—it does not call private `drive_run`. The fixture symbol is absent from production generation. |
| 4 | Cancelling a pending `nextEvent()` consumes no event | Rust `cancelled_pull_does_not_consume` uses a gated runtime and drops a pending receive future. Swift `testCancelledNextEventDoesNotConsume` starts and cancels a `Task`, releases the gate, calls `nextEvent()` again, and asserts the unique envelope is returned. |
| 5 | `outcome()` without draining more than 128 events cannot deadlock | Rust `outcome_closes_undrained_observations` and Swift `testOutcomeWithoutDrainDoesNotDeadlock` script at least 129 events, call `outcome()` immediately, and enforce a short timeout. They cover receiver close, buffered discard, blocked-sender wakeup, and cached result. |
| 6 | Sink-only delivery beyond 128 events has no hidden drainer | Rust `sink_only_run_has_no_observation_sender` scripts at least 129 events, never pulls, counts every sink call, and awaits `outcome`; a crate-private assertion beside `RunChannels` in `crates/pi-agent-runtime-tokio/src/lib.rs` verifies its event sender is `None`. The test-only fixture module exposes that Rust-owned counting sink's start/result controls, and Swift `testSinkOnlyBeyondCapacityNeedsNoDrainer` drives those controls without implementing the conditionally exported host sink. |
| 7 | A held `RunFinished` sink blocks post-terminal EOF, `outcome`, and `waitForIdle`, but not delivery of `RunFinished` itself | Rust `run_finished_sink_blocks_all_settlement` extends the existing basis at `crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:394`. The test-only fixture supplies a Rust-owned gated sink that holds the envelope containing `RunFinished`, plus a `releaseRunFinishedSink()` control. Swift `testRunFinishedSinkBlocksSettlement` first proves that `nextEvent()` observes `RunFinished` while the sink is still held, because the actor sends to observation before awaiting registered and run-scoped sinks (`crates/pi-agent-runtime-tokio/src/lib.rs:838`, `crates/pi-agent-runtime-tokio/src/lib.rs:845`, `crates/pi-agent-runtime-tokio/src/lib.rs:849`). It then starts the **post-terminal EOF pull** and proves that pull, `outcome()`, and `waitForIdle()` remain pending until the gate is released; afterward EOF is returned and both settlement calls complete. This tests generated pull/settlement semantics even if the Swift-authored sink surface is still gated. |
| 8 | Cancellation from inside a Swift sink | Conditional on the owned-`CancellationToken` async-trait generation gate in section 4.4. Rust `sink_can_cancel_with_supplied_token` and Swift `testSinkCanCancelWithSuppliedToken` call the supplied `CancellationToken.cancel()` on a selected envelope, return from the sink, and assert terminal `RunOutcome::Cancelled`; neither calls a mailbox method from the sink. If generation fails, the sink milestone is reported blocked rather than substituting a different contract. |
| 9 | Concurrent `nextEvent()` calls are serialized or rejected | This design selects serialization. Rust `concurrent_pulls_are_serialized` stress-tests the receiver mutex. Swift `testConcurrentNextEventCallsAreSerialized` launches two tasks, merges their unique envelopes, and proves each sequence appears exactly once in source order. |
| 10 | No authoritative Agent `EventSubscription` appears in the generated boundary | CI test `generated_surface_has_no_authoritative_subscription` runs generation and scans generated Swift/C/Rust glue for `EventSubscription<AgentEventEnvelope>`, `AsyncStream<AgentEventEnvelope>`, `EventSubscription<AgentEvent>`, `AsyncStream<AgentEvent>`, `EventSubscription<AssistantEvent>`, and `AsyncStream<AssistantEvent>`. Swift compile test `testGeneratedSurfaceUsesNextEvent` asserts the envelope pull method exists. |
| 11 | Rust-owned Tokio runtime lives through actor shutdown | Rust `owned_runtime_outlives_actor_shutdown` and Swift `testOwnedRuntimeLivesThroughShutdown` create a handle, drop the external `TokioRuntimeOwner`, perform a Tokio-dependent timer operation, and call `shutdown()`. Instrumented supervisor/lease state proves the actor future itself holds a lease, `shutdown()` waits for actor-done, and the runtime owner thread tears down only afterward. A second Rust case drops the handle without explicit shutdown and proves mailbox closure still lets the actor release the last task lease safely. |
| 12 | Envelope sequences are exact across runs and persistence round-trips | Rust `envelope_sequence_spans_runs_and_round_trips` extends `crates/pi-agent-core/tests/m2_1_state.rs:305`: run twice, assert one consecutive global sequence with stable `run_id` grouping, serialize/deserialize and replay every envelope, then compare exact values. Swift `testEnvelopeSequenceAcrossRuns` verifies the same received sequence. This test is mandatory because section 4.5 selects envelopes. |
| 13 | Direct `AssistantEvent` pull is lossless under slow consumption | Rust `assistant_pull_more_than_capacity_is_lossless` and Swift `testSlowAssistantPullHasNoLossOrReordering` configure a test `ChatApi` in the concrete `Models` control plane to return at least three channel capacities of uniquely indexed deltas, delay every pull, and compare the exact complete sequence through `TokioRuntimeOwner::stream_model` and `TokioAssistantStream::next_event`. The production path, rather than a raw-stream helper, is under test. |
| 14 | Direct assistant terminal and EOF semantics are distinct | Rust `assistant_terminal_precedes_validated_eof` configures test `ChatApi` implementations in `Models` and asserts `Finished`, `Failed`, and `Cancelled` are returned before `Ok(None)`. Rust `assistant_raw_eof_is_protocol_error` uses a test `ChatApi` returning `AssistantStream::new` over a nonterminal event and raw EOF, then asserts `MissingTerminalEvent`. The `boltffi-test-fixtures`-only generated factory builds that same concrete `Models` path for Swift; `testAssistantRawEofThrows` proves the final `nextEvent()` throws rather than returning `nil`. The three `AssistantEvent` terminal variants are defined at `crates/pi-ai/src/streaming.rs:521`, `crates/pi-ai/src/streaming.rs:528`, and `crates/pi-ai/src/streaming.rs:534`. |
| 15 | Cancelling a pending direct assistant pull consumes no event | Rust `cancelled_assistant_pull_does_not_consume` and Swift `testCancelledAssistantNextEventDoesNotConsume` gate the producer, cancel a pending `nextEvent()` task, release one uniquely identified event, and assert the following pull returns it. |
| 16 | Direct model cancellation covers establishment, lifecycle-preserving, and abandonment paths | One Rust/Swift case cancels the task awaiting `streamModel(request:)` before establishment and proves its guard cancels the token, closes observation, and releases the producer lease. Established-stream cases call `cancel()` and continue pulling to `AssistantEvent::Cancelled`; separate cases fill the event channel, call `cancelAndWait()` without resuming pulls, and assert sender wakeup, internal terminal validation, producer completion, and runtime-lease release without deadlock. |
| 17 | Generated Swift can construct a production configured `Models` and actor without provider-authoring traits or a fixture | Rust `providers/pi-ai-openai/tests/native_models_factory.rs::factory_builds_openai_models_with_native_transport_and_auth` calls the added `OpenAiModelsFactory`, includes a compile-time `NativeOpenAiHttpTransport: HttpTransport` assertion, asserts the OpenAI catalog is present, and resolves the seeded API-key auth through the canonical `Models` control plane without exposing the key. Swift `testProductionOpenAIConstruction` calls the generated `OpenAiModelsFactory(apiKey:)` and `build()`, constructs canonical `AgentState` for a pinned OpenAI catalog entry, creates `ToolRegistry` and `TokioRuntimeOwner`, calls `spawnAgent`, reads the initial snapshot, and awaits `shutdown`. It performs no live network request, imports no test-fixture feature, and compile-time surface checks reject any `ProviderRegistration`, `HttpTransport`, `AuthResolver`, `ModelCatalog`, or `ChatApi` argument in that call chain. |
| 18 | Every transitive JSON/map/tuple-newtype/tuple-payload-data-enum/replay path in section 8.2 generates and preserves its canonical value | Rust `crates/pi-agent-runtime-tokio/tests/boltffi_owned_value_graph.rs` constructs every path as a concrete root, never inferring an event or snapshot path from a request or committed transcript. The roots are: (a) a direct `ModelRequest` whose context contains assistant and tool-result messages with `ToolCall.arguments`, `DeferredHandle.data`, diagnostic number/details, `Timestamp`, `Currency` through cost, `ToolResultMessage.details -> VersionedExtension.value`, and a `ToolSpec.parameters` plus configured `ConstrainedSampling`/`GrammarVariants`; (b) standalone `AssistantEvent::DiagnosticAdded`; (c) standalone `AssistantEvent::{Finished,Failed,Cancelled}`, each with a terminal message whose `ReplayEnvelope` is independently nonempty and exercised by the matrix below; (d) nested `AgentEvent::AssistantUpdate` values for those four cases; (e) explicit assistant and tool-result `AgentEvent::MessageCommitted` roots; (f) a standalone assistant `AgentRecord`; (g) a direct `AgentState` whose transcript contains an assistant record; (h) `ToolExecutionStarted`, `ToolExecutionUpdated`, and `ToolExecutionFinished`; (i) `AgentEvent::ContextPrepared` with `OpaqueReplayDropped { reason: ReplayDropReason }`; and (j) an `AgentSnapshot` containing committed records **and** `streaming: Some(AssistantMessageSnapshot)` whose deferred data, diagnostic number/details, partial tool-call arguments, usage/cost, timestamp, and terminal message use sentinels deliberately different from `state.transcript` (`crates/pi-ai/src/streaming.rs:428`, `crates/pi-ai/src/streaming.rs:522`, `crates/pi-ai/src/streaming.rs:528`, `crates/pi-ai/src/streaming.rs:534`, `crates/pi-agent-core/src/events.rs:97`, `crates/pi-agent-core/src/events.rs:113`, `crates/pi-agent-core/src/events.rs:120`, `crates/pi-agent-core/src/state.rs:188`, `crates/pi-ai/src/streaming.rs:1689`, `crates/pi-ai/src/streaming.rs:1693`, `crates/pi-ai/src/streaming.rs:1695`, `crates/pi-ai/src/streaming.rs:1699`, `crates/pi-ai/src/streaming.rs:1701`, `crates/pi-ai/src/streaming.rs:1703`, `crates/pi-ai/src/streaming.rs:1705`). Replay envelope coverage is the thirteen-root matrix specified immediately below. Within that matrix, `AgentSnapshot.streaming.replay` and `AgentSnapshot.streaming.terminal_message.replay` each contain a different `ReplayEnvelope` with a different fully populated `ReplayScope`—distinct `provider`, `api`, `requested_model`, `produced_by_model`, and `protocol_revision` sentinels—a nonempty ordered `Vec<ReplayItem>`, and root-specific sentinels for every envelope/item field (`crates/pi-ai/src/streaming.rs:1697`, `crates/pi-ai/src/streaming.rs:1705`, `crates/pi-ai/src/messages.rs:157`, `crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:88`, `crates/pi-ai/src/replay.rs:90`, `crates/pi-ai/src/replay.rs:92`, `crates/pi-ai/src/replay.rs:94`, `crates/pi-ai/src/replay.rs:96`, `crates/pi-ai/src/replay.rs:98`, `crates/pi-ai/src/replay.rs:135`). Like every root in the thirteen-root matrix, each of those two envelopes independently includes all four `ReplayTarget` forms—`Message`, `ContentBlock`, `ToolCall`, and `ProviderOutputItem`—and all three `OpaquePayload` forms—`Utf8`, `Bytes`, and `JsonBytes`—using distinct strings, IDs, indices, raw byte sequences, JSON byte sequences, ordinals, kinds, applicability values, and completeness values; Swift asserts the exact schema version, every scope field, ordered item count, and every item field/payload byte (`crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`). Direct replay-event roots are also exhaustive: create standalone `AssistantEvent::ReplayItemStarted` values for all four target forms and standalone `AssistantEvent::ReplayData` values for all five `ReplayDataOperation::{ReplaceUtf8,AppendUtf8,ReplaceBytes,AppendBytes,ReplaceJsonBytes}` forms, then create a corresponding nested `AgentEvent::AssistantUpdate` for every direct value with distinct `message_id`, `item_id`, target-ID/index, and operation-payload sentinels; Swift asserts exact case and payload fidelity for every direct and nested root (`crates/pi-ai/src/streaming.rs:474`, `crates/pi-ai/src/streaming.rs:488`, `crates/pi-ai/src/streaming.rs:563`, `crates/pi-agent-core/src/events.rs:113`). Separate direct generation/round-trip probes cover tuple newtypes—macro IDs, `QueueSequence`, `EventSinkId`, `Timestamp`, `Currency`, `ReplayDropReason`, `OrderedJsonString`, `OrderedJsonObject`, and `OrderedJsonArray`—and the ordered-JSON case includes nested arrays, insertion-ordered objects, and exact UTF-16 code units (`crates/pi-ai/src/ids.rs:6`, `crates/pi-agent-core/src/control.rs:19`, `crates/pi-agent-runtime-tokio/src/lib.rs:35`, `crates/pi-ai/src/ids.rs:133`, `crates/pi-ai/src/usage.rs:88`, `crates/pi-ai/src/handoff.rs:44`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227`). A second explicit matrix generates and round-trips every variant of the tuple-payload data enums: `Message::{User,Assistant,ToolResult}`, `AgentRecord::{Llm,Custom}`, `DiagnosticErrorCode::{String,Number}`, `ConstrainedSampling::{Disabled,Config}`, `OrderedJsonValue::{Absent,Null,Bool,Number,String,Array,Object}`, all four `ReplayTarget` variants, all three `OpaquePayload` variants, and all five `ReplayDataOperation` variants (`crates/pi-ai/src/messages.rs:32`, `crates/pi-agent-core/src/state.rs:62`, `crates/pi-ai/src/messages.rs:178`, `crates/pi-ai/src/messages.rs:334`, `crates/pi-ai/src/json_compat.rs:324`, `crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:276`, `crates/pi-ai/src/streaming.rs:563`). It also covers request headers, API patches, and custom records. Swift `testOwnedValueGraphIsCompleteAndLossless` sends every root through generated constructors/methods or the test-only identity fixture and asserts exact canonical variant identity and payload fidelity, `serde_json::Value` equality, exact `RawValue` text, every map entry/order, exact tuple payloads/UTF-16 units, all terminal cases, committed records, direct-request fields, standalone assistant `AgentRecord`, direct `AgentState.transcript`, `AgentSnapshot.state`, all thirteen independently sentinelized replay-envelope roots, and every direct/nested replay event. The fixture only constructs/returns canonical types. A missing mapping blocks by returning to the canonical inline type design; the test may not substitute strings, duplicate records, a binding-only enum, or a binding-only envelope. |
| 19 | Cancelling an exported Agent run-establishment future after actor acceptance but before `TokioAgentRun` handoff cancels and settles the otherwise unobservable run | Rust `cancelled_agent_establishment_after_acceptance_settles` lives in the `#[cfg(test)]` unit module beside `request_run` and `accept_run` (`crates/pi-agent-runtime-tokio/src/lib.rs:394`, `crates/pi-agent-runtime-tokio/src/lib.rs:697`). A test-only handoff gate signals only after `accepted_rx` has yielded `Ok(())` and then holds the establishment future immediately before `RunEstablishmentGuard::handoff`; the scripted core run remains pending until its shared token is cancelled. The test drops the still-pending establishment future, releases no consumer, and proves: the guard token became cancelled; an acknowledged Rust probe observed the terminal `AgentEvent::RunFinished { outcome: RunOutcome::Cancelled { .. } }`; `wait_for_idle()` completed; and the runtime supervisor's live-task lease count returned to its actor-only pre-run baseline. It then calls orderly `shutdown()`, awaits actor-done, and proves the last actor lease was released and the owner thread could tear down the runtime. Run this table for `prompt_text`, concrete `prompt_records`, `continue_run`, and `retry_last_turn`, with the state preconditioned for continue/retry; also run the `prompt_text_with_sink` case when that conditional surface is enabled. The generated fixture exposes only the same test gate/probe controls. Swift `testCancelledAgentEstablishmentAfterAcceptanceSettles` starts each generated run-establishment call in a `Task`, waits for the post-acceptance/pre-handoff signal, cancels the task, proves that no `TokioAgentRun` was returned, and then asserts the same cancellation settlement and actor-idle observations through generated fixture methods; after orderly generated `shutdown()`, it proves the actor lease was released and runtime teardown completed. The gate makes the race deterministic; a timeout, receiver-drop-only completion, or forced shutdown in place of cancellation settlement fails the test. |

Acceptance test 18 also has a mandatory, independent `usize` error-payload
gate. Rust test `control_error_queue_full_usize_payload_is_exact` constructs
`ControlError::QueueFull` with zero, the current event-capacity value, and
`usize::MAX`, and checks the exact stored values
(`crates/pi-agent-core/src/control.rs:68`,
`crates/pi-agent-core/src/control.rs:74`,
`crates/pi-agent-runtime-tokio/src/lib.rs:33`). The test-only generated fixture
throws those same three canonical `ControlError` values. Swift
`testControlErrorQueueFullCapacityIsExact` catches the generated error,
pattern-matches `QueueFull`, and asserts each exact capacity value using the
actual declaration generation produced; any narrowing, omission, string
flattening, or lost payload fails the gate. The numeric quick-reference table
does not list `usize`, and the isolated `usize` function-argument example does
not establish error-variant fields. **UNRESOLVED: not answered by the
documentation**. Pages checked: `types.md#quick-reference`,
`types.md#primitives`, `errors.md#enum-errors`, and
`errors.md#enums-with-payloads`.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors]
[https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]

The same test's generated-surface audit asserts that
`DEFAULT_COMMAND_CAPACITY` and `DEFAULT_EVENT_CAPACITY` are absent from the
production Swift/C/Rust glue, because phase 2 deliberately keeps both current
`usize` constants unannotated (`crates/pi-agent-runtime-tokio/src/lib.rs:30`,
`crates/pi-agent-runtime-tokio/src/lib.rs:33`). Rust tests may use those
constants internally. The constants page does not establish `usize` constants.
**UNRESOLVED: not answered by the documentation**. Pages checked:
`constants.md#supported-values`, `types.md#quick-reference`, and
`types.md#primitives`.
[https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]

Acceptance test 18 has this mandatory replay-envelope matrix; every numbered
root is constructed both in the Rust test and in the test-only canonical-value
fixture consumed by Swift:

1. Direct `ModelRequest.context.messages` assistant replay
   (`crates/pi-ai/src/runtime.rs:17`, `crates/pi-ai/src/messages.rs:473`,
   `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
2. Direct `AssistantEvent::Finished.message.replay`
   (`crates/pi-ai/src/streaming.rs:521`,
   `crates/pi-ai/src/messages.rs:157`).
3. Direct `AssistantEvent::Failed.message.replay`
   (`crates/pi-ai/src/streaming.rs:528`,
   `crates/pi-ai/src/messages.rs:157`).
4. Direct `AssistantEvent::Cancelled.message.replay`
   (`crates/pi-ai/src/streaming.rs:534`,
   `crates/pi-ai/src/messages.rs:157`).
5. `AgentEvent::AssistantUpdate` carrying `Finished`, through its terminal
   message replay
   (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:521`,
   `crates/pi-ai/src/messages.rs:157`).
6. `AgentEvent::AssistantUpdate` carrying `Failed`, through its terminal
   message replay
   (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:528`,
   `crates/pi-ai/src/messages.rs:157`).
7. `AgentEvent::AssistantUpdate` carrying `Cancelled`, through its terminal
   message replay
   (`crates/pi-agent-core/src/events.rs:113`,
   `crates/pi-ai/src/streaming.rs:534`,
   `crates/pi-ai/src/messages.rs:157`).
8. `AgentEvent::MessageCommitted` carrying
   `AgentRecord::Llm(Message::Assistant(_))`
   (`crates/pi-agent-core/src/events.rs:120`,
   `crates/pi-agent-core/src/state.rs:64`,
   `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
9. Standalone assistant `AgentRecord` replay
   (`crates/pi-agent-core/src/state.rs:64`,
   `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
10. Direct `AgentState.transcript` assistant replay
    (`crates/pi-agent-core/src/state.rs:33`,
    `crates/pi-agent-core/src/state.rs:64`,
    `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
11. `AgentSnapshot.state.transcript` assistant replay
    (`crates/pi-agent-core/src/state.rs:184`,
    `crates/pi-agent-core/src/state.rs:33`,
    `crates/pi-agent-core/src/state.rs:64`,
    `crates/pi-ai/src/messages.rs:36`, `crates/pi-ai/src/messages.rs:157`).
12. `AgentSnapshot.streaming.replay`
    (`crates/pi-agent-core/src/state.rs:188`,
    `crates/pi-ai/src/streaming.rs:1697`).
13. `AgentSnapshot.streaming.terminal_message.replay`
    (`crates/pi-agent-core/src/state.rs:188`,
    `crates/pi-ai/src/streaming.rs:1705`,
    `crates/pi-ai/src/messages.rs:157`).

For each root, the Rust test and Swift assertion use a unique root prefix in
all `ReplayScope` strings and every item ID/kind/target identifier or index,
plus different raw and JSON byte sequences. The envelope is nonempty and its
ordered items collectively exercise all four `ReplayTarget` variants and all
three `OpaquePayload` variants. The assertion compares schema version, all five
scope fields, item count and order, and every item ID, ordinal, target, kind,
applicability, completeness, and exact payload string/bytes
(`crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:88`,
`crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:179`,
`crates/pi-ai/src/replay.rs:276`). A zero-item envelope fails immediately. The
separate direct/nested `ReplayItemStarted` and `ReplayData` cases still exercise
all four targets and all five operations with their own sentinels; they do not
count as any numbered envelope root (`crates/pi-ai/src/streaming.rs:474`,
`crates/pi-ai/src/streaming.rs:488`,
`crates/pi-ai/src/streaming.rs:563`,
`crates/pi-agent-core/src/events.rs:113`).

Tests 1–12 are the adopted owner-review requirements; tests 13–16 close the
direct model-stream R3 contract required by this revision; test 17 closes the
production construction path; test 18 closes the transitive-value generation
gates; and test 19 closes the accepted-Agent-run establishment cancellation
race. Every acceptance test is semantic. Generated symbol presence alone is
insufficient.

## 10. Phased implementation plan

No phase publishes a crate or changes the current UniFFI artifact. Failure at a
phase gate stops expansion.

1. **Canonical Rust changes.** Reshape `TokioAgentRun` with interior
   synchronization, cached watch-style completion, `&self` pull/outcome,
   `cancel`, `cancel_and_outcome`, EOF validation, and Rust-only raw receiver
   access. Move run-token creation into `request_run` and add the armed
   `RunEstablishmentGuard` from section 4.1: it owns unhanded observation and
   completion receivers plus a shared-token clone, cancels and closes them on
   every pre-handoff drop, and disarms only in the final non-awaiting
   `TokioAgentRun` return step. Make sink-only observation optional. Rewrite
   `AgentEventSink` to async-trait form without changing barrier ordering. Add
   `TokioRuntimeOwner::new(&Models, &ToolRegistry)` and
   `spawn_agent(AgentState)`; make the actor future capture a runtime lease and
   make `shutdown()` await actor-done. Add `TokioAssistantStream`, the
   runtime-owned `stream_model(ModelRequest)` producer, validated direct EOF,
   cancellation, and `cancel_and_wait`. Add the concrete `Vec<AgentRecord>`
   prompt path and the inherent canonical `Models::new()` constructor. Add
   `NativeOpenAiHttpTransport`, `OpenAiModelsFactory`, its sanitized concrete
   error, and `InMemoryCredentialStore::with_credential` so ordinary Rust and
   Swift have one real provider/transport/auth construction path returning
   canonical `Models`. Promote
   `AgentEventEnvelope` into the canonical Tokio observation channel and sink
   contract, allocating one envelope before fan-out. Put the malformed Agent
   event stream test beside private `drive_run`; do not attempt to manufacture
   that failure through `ScriptedRuntime`. Add the deterministic test-19 hook
   that pauses the establishment future after actor acceptance and immediately
   before guard handoff; its ordinary Rust test must drop that future and prove
   terminal cancellation settlement, actor idleness, return of the live-task
   lease count to the actor-only baseline, and release of the final actor lease
   after orderly shutdown for prompt/continue/retry. Acceptance: ordinary Rust
   tests for items 1–19, except generated-language halves, pass with no BoltFFI
   dependency enabled. Phase 1 fails if an accepted run can outlive its dropped
   establishment future unobserved. Keep `DEFAULT_COMMAND_CAPACITY` and
   `DEFAULT_EVENT_CAPACITY` as their current canonical Rust-only `usize`
   constants; this design does not select a fixed-width API change merely to
   export them (`crates/pi-agent-runtime-tokio/src/lib.rs:30`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:33`).

2. **Inline annotations and generation smoke tests.** Apply the documented
   integration pieces to the canonical crates only: the normal and build
   dependencies, `staticlib` crate type, `build.rs` generation call, and
   `boltffi check` are the documented installation flow.
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]
   Add the documented root `boltffi.toml` package/source-crate configuration.
   [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
   Apply `#[data]` to supported canonical named-field records and documented
   unit/struct-style enum shapes, `#[error]` to supported canonical errors,
   `#[export]` to canonical class impls, and `#[export]` plus
   `#[async_trait]` to the conditional acknowledged sink. Do not apply
   `#[data]` to a tuple-payload data enum until its separate generation probe
   succeeds: the records documentation does not show tuple-variant syntax.
   **UNRESOLVED: not answered by the documentation**. Pages checked:
   `records.md#enums`, `records.md#enums-with-associated-data`, and
   `types.md#records`.
   [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs]
   [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums]
   [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#records]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#supported-error-types]
   [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#traits]
   [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]

   Feature gating is a separate generation experiment, not a documented
   capability. Whether `cfg_attr`, optional BoltFFI/build dependencies, and
   discovery of annotations across `pi-ai`, `pi-ai-openai`,
   `pi-agent-core`, and `pi-agent-runtime-tokio` work is
   **UNRESOLVED: not answered by the documentation**; the pages checked are
   `installation.md#add-to-your-project`, `installation.md#create-buildrs`,
   `getting-started.md#write-your-code`, and
   `configuration.md#package-identity`.
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
   [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code]
   [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
   This experiment must resolve or report those three questions before broad
   annotation; no generated-package layout may be assumed from the snapshot.
   Start with `Models`, `OpenAiModelsFactory`, `OpenAiModelsError`,
   `ToolRegistry`, `AgentState`, one documented-shape error,
   `TokioRuntimeOwner`, `PromptText`, reshaped `TokioAgentRun`, and
   `TokioAssistantStream`. Generate and execute the production construction
   chain from acceptance test 17 before any test-only factory. Add the
   `boltffi-test-fixtures`-only malformed Agent and Assistant constructors and
   the post-acceptance/pre-handoff Agent establishment gate/probe controls to
   the test generation target, never the production target. Separately
   generate `TokioAgentError` and compile a Swift
   throwing/catching fixture that exercises both its tuple-payload `Agent`
   variant and the nested `AgentError`; section 8.3 permits neither shape to be
   assumed. Separately generate `AgentEvent`, `AssistantEvent`,
   `RequestStartErrorKind` inside `RequestStartError`, `TokioAgentError`,
   `AgentError`, and `ControlError`; compile Swift switches/catches for all six
   `#[non_exhaustive]` occurrences because section 8.4 permits none to be
   inferred from another. Independently generate
   `ControlError::QueueFull { capacity: usize }` and run acceptance test 18's
   generated Swift catch and exact-payload-fidelity cases before enabling any
   method that can throw `ControlError`
   (`crates/pi-agent-core/src/control.rs:68`,
   `crates/pi-agent-core/src/control.rs:74`). The numeric quick-reference table
   omits `usize`; the isolated `usize` function-argument example does not answer
   error-payload support. **UNRESOLVED: not answered by the documentation**.
   Pages checked: `types.md#quick-reference`, `types.md#primitives`, and
   `errors.md#enums-with-payloads`.
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
   [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads]
   Keep `DEFAULT_COMMAND_CAPACITY` and `DEFAULT_EVENT_CAPACITY` unannotated,
   and make the generated-surface half of acceptance test 18 fail if either
   name appears in production glue. Both are currently `usize`
   (`crates/pi-agent-runtime-tokio/src/lib.rs:30`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:33`), and the documentation does
   not establish `usize` constants. **UNRESOLVED: not answered by the
   documentation**. Pages checked: `constants.md#supported-values`,
   `types.md#quick-reference`, and `types.md#primitives`.
   [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]
   [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives]
   Generate a minimal async sink separately: the sink
   surface advances only if the owned-`CancellationToken` async-trait method is
   proven. Separately generate and compile exhaustive Swift construction,
   switching, and exact-payload round trips for `Message`, `AgentRecord`,
   `DiagnosticErrorCode`, `ConstrainedSampling`, `OrderedJsonValue`,
   `ReplayTarget`, `OpaquePayload`, and `ReplayDataOperation`; every variant of
   every enum must pass before its enclosing request/event/snapshot surface
   advances (`crates/pi-ai/src/messages.rs:32`,
   `crates/pi-agent-core/src/state.rs:62`,
   `crates/pi-ai/src/messages.rs:178`,
   `crates/pi-ai/src/messages.rs:334`,
   `crates/pi-ai/src/json_compat.rs:324`,
   `crates/pi-ai/src/replay.rs:179`,
   `crates/pi-ai/src/replay.rs:276`,
   `crates/pi-ai/src/streaming.rs:563`). No binding-only replacement enum or
   envelope is permitted.

   The transitive-value gate is path-specific. Generate and compile Swift
   access for these canonical roots before annotating any enclosing production
   root as complete:

   - For `serde_json::Value`, cover `ToolCall.arguments` in a terminal
     `AssistantEvent`, `AgentEvent::ToolExecutionStarted`, a committed
     `AgentRecord`/`AgentSnapshot.state`, the independent
     `AgentSnapshot.streaming.content` and `.terminal_message` paths, and
     direct `ModelRequest.context`; cover `ToolSpec.parameters` through
     `Context.tools`; cover
     `DeferredHandle.data` through all three assistant terminal variants,
     committed records/`AgentSnapshot.state`, the independent
     `AgentSnapshot.streaming.deferred` and `.terminal_message` paths, and
     later direct `ModelRequest.context`; and cover diagnostic details through
     explicit `DiagnosticAdded`, all terminal messages, committed records/
     `AgentSnapshot.state`, the independent
     `AgentSnapshot.streaming.diagnostics` and `.terminal_message` paths, and
     later direct requests.
   - For `RawValue`, cover `AgentRecord::Custom`,
     `ErasedApiOptionsPatch::value`, `ToolUpdate.details` in
     `ToolExecutionUpdated`, `ToolOutput.details` in `ToolExecutionFinished`,
     and `ToolResultMessage.details -> VersionedExtension.value` in
     explicit `AgentEvent::MessageCommitted`, `AgentState`,
     `AgentSnapshot.state`, and later direct `ModelRequest.context` values.
   - For `BTreeMap`, cover `HeaderMapSpec`, diagnostic details, and
     `GrammarVariants` inside a `ToolSpec` carried by direct
     `ModelRequest.context`; cover diagnostic details in direct requests as
     well as direct/nested events, records, `AgentSnapshot.state`, and the
     independent `AgentSnapshot.streaming.diagnostics` and
     `.terminal_message` paths.
   - Generate the actual enum roots separately: standalone
     `AssistantEvent::DiagnosticAdded`; standalone `Finished`, `Failed`, and
     `Cancelled`; nested `AgentEvent::AssistantUpdate` values carrying each of
     those four assistant cases; explicit assistant and tool-result
     `AgentEvent::MessageCommitted` values; and `AgentEvent::ContextPrepared`
     with an `OpaqueReplayDropped` change. Do not infer any of these paths from
     a direct request or a transcript record.
   - Generate and compile the complete replay-envelope matrix as thirteen
     separately constructed canonical roots:

     1. Direct `ModelRequest.context.messages` assistant replay
        (`crates/pi-ai/src/runtime.rs:17`,
        `crates/pi-ai/src/messages.rs:473`,
        `crates/pi-ai/src/messages.rs:36`,
        `crates/pi-ai/src/messages.rs:157`).
     2. Direct `AssistantEvent::Finished.message.replay`
        (`crates/pi-ai/src/streaming.rs:521`,
        `crates/pi-ai/src/messages.rs:157`).
     3. Direct `AssistantEvent::Failed.message.replay`
        (`crates/pi-ai/src/streaming.rs:528`,
        `crates/pi-ai/src/messages.rs:157`).
     4. Direct `AssistantEvent::Cancelled.message.replay`
        (`crates/pi-ai/src/streaming.rs:534`,
        `crates/pi-ai/src/messages.rs:157`).
     5. `AgentEvent::AssistantUpdate` carrying `Finished`, through its terminal
        message replay
        (`crates/pi-agent-core/src/events.rs:113`,
        `crates/pi-ai/src/streaming.rs:521`,
        `crates/pi-ai/src/messages.rs:157`).
     6. `AgentEvent::AssistantUpdate` carrying `Failed`, through its terminal
        message replay
        (`crates/pi-agent-core/src/events.rs:113`,
        `crates/pi-ai/src/streaming.rs:528`,
        `crates/pi-ai/src/messages.rs:157`).
     7. `AgentEvent::AssistantUpdate` carrying `Cancelled`, through its terminal
        message replay
        (`crates/pi-agent-core/src/events.rs:113`,
        `crates/pi-ai/src/streaming.rs:534`,
        `crates/pi-ai/src/messages.rs:157`).
     8. `AgentEvent::MessageCommitted` carrying
        `AgentRecord::Llm(Message::Assistant(_))`, through that assistant's
        replay
        (`crates/pi-agent-core/src/events.rs:120`,
        `crates/pi-agent-core/src/state.rs:64`,
        `crates/pi-ai/src/messages.rs:36`,
        `crates/pi-ai/src/messages.rs:157`).
     9. Standalone assistant `AgentRecord` replay
        (`crates/pi-agent-core/src/state.rs:64`,
        `crates/pi-ai/src/messages.rs:36`,
        `crates/pi-ai/src/messages.rs:157`).
     10. Direct `AgentState.transcript` assistant replay
         (`crates/pi-agent-core/src/state.rs:33`,
         `crates/pi-agent-core/src/state.rs:64`,
         `crates/pi-ai/src/messages.rs:36`,
         `crates/pi-ai/src/messages.rs:157`).
     11. `AgentSnapshot.state.transcript` assistant replay
         (`crates/pi-agent-core/src/state.rs:184`,
         `crates/pi-agent-core/src/state.rs:33`,
         `crates/pi-agent-core/src/state.rs:64`,
         `crates/pi-ai/src/messages.rs:36`,
         `crates/pi-ai/src/messages.rs:157`).
     12. `AgentSnapshot.streaming.replay`
         (`crates/pi-agent-core/src/state.rs:188`,
         `crates/pi-ai/src/streaming.rs:1697`).
     13. `AgentSnapshot.streaming.terminal_message.replay`
         (`crates/pi-agent-core/src/state.rs:188`,
         `crates/pi-ai/src/streaming.rs:1705`,
         `crates/pi-ai/src/messages.rs:157`).

     Give every root a distinct, nonempty `ReplayEnvelope` with the exact
     canonical schema version, root-specific sentinels in all five
     `ReplayScope` fields, ordered items, every item field, all four
     `ReplayTarget` variants, and all three
     `OpaquePayload` variants. Compile Swift construction/switching and compare
     exact strings, IDs, indexes, raw bytes, JSON bytes, ordinals, kinds,
     applicability, and completeness for each root independently
     (`crates/pi-ai/src/replay.rs:13`,
     `crates/pi-ai/src/replay.rs:88`,
     `crates/pi-ai/src/replay.rs:135`,
     `crates/pi-ai/src/replay.rs:179`,
     `crates/pi-ai/src/replay.rs:276`). A root with zero items, shared sentinel
     data, or coverage inferred from another root fails phase 2.
   - Generate replay events as a separate direct/nested matrix: standalone
     `AssistantEvent::ReplayItemStarted` for each of the four `ReplayTarget`
     forms and standalone `AssistantEvent::ReplayData` for each of the five
     `ReplayDataOperation` forms, plus a corresponding
     `AgentEvent::AssistantUpdate` for every case. Use different sentinels for
     direct and nested roots and assert exact Swift case/payload fidelity
     (`crates/pi-ai/src/streaming.rs:474`,
     `crates/pi-ai/src/streaming.rs:488`,
     `crates/pi-ai/src/streaming.rs:563`,
     `crates/pi-agent-core/src/events.rs:113`).
   - Construct `AgentSnapshot.streaming` as
     `Some(AssistantMessageSnapshot)` with deferred data, diagnostic number and
     details, partial tool-call arguments, usage/cost, timestamp, and a
     terminal message whose sentinel values differ from
     `AgentSnapshot.state.transcript`. Populate `streaming.replay` and
     `streaming.terminal_message.replay` as two distinct, nonempty
     `ReplayEnvelope` values. Each envelope must use a separately identifiable
     `ReplayScope`, preserve item order and every item field, and independently
     contain all four `ReplayTarget` forms and all three
     `OpaquePayload::{Utf8,Bytes,JsonBytes}` forms with distinct string, ID,
     index, byte, and JSON-byte sentinels. Validate every active and nested
     replay field after the generated Swift round trip
     (`crates/pi-agent-core/src/state.rs:188`,
     `crates/pi-ai/src/streaming.rs:1689`,
     `crates/pi-ai/src/streaming.rs:1693`,
     `crates/pi-ai/src/streaming.rs:1695`,
     `crates/pi-ai/src/streaming.rs:1697`,
     `crates/pi-ai/src/streaming.rs:1699`,
     `crates/pi-ai/src/streaming.rs:1701`,
     `crates/pi-ai/src/streaming.rs:1703`,
     `crates/pi-ai/src/streaming.rs:1705`,
     `crates/pi-ai/src/messages.rs:157`,
     `crates/pi-ai/src/replay.rs:13`,
     `crates/pi-ai/src/replay.rs:88`,
     `crates/pi-ai/src/replay.rs:135`,
     `crates/pi-ai/src/replay.rs:179`,
     `crates/pi-ai/src/replay.rs:276`). An empty replay envelope is not a
     fidelity test.
   - Retain separate probes for `serde_json::Number`, `BTreeSet`,
     `Arc<[T]>`, `IndexMap`, and `i128`. Replace the former tuple-ID-only probe
     with independent generation and exact-fidelity probes for macro IDs,
     `QueueSequence`, `EventSinkId`, `Timestamp`, `Currency`,
     `ReplayDropReason`, `OrderedJsonString`, `OrderedJsonObject`, and
     `OrderedJsonArray`. Exercise ordered JSON recursively with exact UTF-16
     units, nested arrays, and insertion-ordered objects; success for an ID or
     the outer sampling object cannot stand in for those inner tuple wrappers
     (`crates/pi-ai/src/ids.rs:6`,
     `crates/pi-agent-core/src/control.rs:19`,
     `crates/pi-agent-runtime-tokio/src/lib.rs:35`,
     `crates/pi-ai/src/ids.rs:133`, `crates/pi-ai/src/usage.rs:88`,
     `crates/pi-ai/src/handoff.rs:44`,
     `crates/pi-ai/src/json_compat.rs:24`,
     `crates/pi-ai/src/json_compat.rs:114`,
     `crates/pi-ai/src/json_compat.rs:227`).

   Acceptance test 18 is the generated Swift and value-fidelity gate for this
   matrix. No successful probe may be generalized to an untested root merely
   because it contains the same leaf type. Acceptance: resolve or report the
   `cfg_attr`, multi-crate discovery,
   tuple-newtype, tuple-payload data-enum, tuple-payload error, nested
   error-valued payload, `ControlError::QueueFull.capacity` `usize` fidelity,
   the complete active/direct/nested replay graph, all six non-exhaustive enum
   occurrences, owned callback argument, and transitive value gaps;
   `boltffi check` and Swift generation pass with no facade or duplicate values.

3. **Swift acceptance suite.** Create the test-only SwiftPM package and
   implement all nineteen tests in section 9 against the generated module.
   Acceptance: every test passes on a deliberately slow consumer and under
   Swift task cancellation; generated-surface test 10 proves no authoritative
   `EventSubscription` exists. Test 18 separately proves the exact generated
   `ControlError::QueueFull.capacity` payload and confirms that the two
   unannotated `usize` capacity constants are absent from production glue
   (`crates/pi-agent-core/src/control.rs:74`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:30`,
   `crates/pi-agent-runtime-tokio/src/lib.rs:33`). Test 19 is a separate
   blocking phase-3 gate: the
   Swift task is cancelled only after the actor-accepted signal and while the
   establishment future is held before result handoff, and the generated test
   must then prove cancellation settlement and actor idleness, followed by
   actor/runtime-lease release on orderly shutdown. Receiver closure alone, a
   timeout, or forced actor shutdown in place of cancellation settlement does
   not satisfy that gate.

4. **Separate callback-authoring milestones.** Add Swift-authored tools,
   policies, storage backends, provider extensions, and other generic
   capabilities only as their canonical Rust APIs acquire concrete,
   exportable contracts. These do not block the initial Agent consumer path.

5. **Apple packaging and migration decision.** Generate Swift source during
   iteration, then use the documented Apple packaging flow for the XCFramework
   and Swift package.
   [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
   Acceptance: the new module works on every configured Apple slice, coexists
   with `pi-ffi`, and is considered for replacement only after all semantic
   tests and repository commitment gates pass.
