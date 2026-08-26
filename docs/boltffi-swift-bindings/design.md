# AgentPrism lower-level Agent Swift SDK — BoltFFI implementation blueprint

Status: implementation-ready design, 2026-08-26.

Authority, in precedence order:

1. `docs/boltffi-swift-bindings/owner-review-2026-08-26b-implementation-audit.md`
   (adopted 2026-08-26).
2. `docs/boltffi-swift-bindings/owner-review-2026-08-26.md`
   (adopted 2026-08-26).
3. `docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part2-revision.md`,
   then Part 1 where Part 2 is silent.

This blueprint uses only the current `agentprism-*` tree. It does not authorize
crate-source edits during the Design workflow phase.

## 1. Decision, scope, and non-negotiable contracts

### 1.1 Scope decision: option 1

This milestone is **option 1: the lower-level Agent Swift SDK**. Its application
boundary is the concrete Tokio actor and the direct model-stream path:

- `TokioAgentHandle`, `TokioAgentRun`, and `TokioAssistantStream`;
- owned `AgentEventEnvelope`, `AgentEvent`, `AssistantEvent`, outcomes,
  snapshots, prompt, queue, and control values;
- `AgentEventSink` only for acknowledged callback delivery;
- `Models`, its auth/catalog control operations needed by an ordinary native
  consumer, provider-neutral native construction, and Rust-owned Tokio
  execution.

The production coding-agent SDK over `agentprism-harness` is a separate future
milestone. The harness now exists in the current workspace (`Cargo.toml:7`),
but durable sessions, harness orchestration, compaction, skills, prompt
templates, environment capabilities, and harness events are deliberately not
claimed by this package.

The actor boundary is selected because `TokioAgentHandle` already owns an
`Agent` on one task and serializes application commands
(`crates/agentprism-runtime-tokio/src/lib.rs:148`). The lower-level
`Agent::{run,prompt_text,prompt_records,continue_run,retry_last_turn}` methods
borrow `&mut Agent` and return borrowed streams
(`crates/agentprism-core/src/run.rs:376`,
`crates/agentprism-core/src/run.rs:385`,
`crates/agentprism-core/src/run.rs:395`,
`crates/agentprism-core/src/run.rs:405`,
`crates/agentprism-core/src/run.rs:429`). They remain Rust composition seams,
not exported classes.

### 1.2 R1–R3

**R1.** Swift receives the same concrete, owned API an ordinary Rust
application uses at this boundary. There is no JSON command dispatcher,
binding-only event envelope, or hand-written Swift behavior layer.

**R2.** No separately maintained FFI facade, duplicate record hierarchy, IDL,
or required Swift wrapper is introduced. Canonical crates may receive inline
BoltFFI annotations and small, legitimate Rust API improvements: owned return
values, concrete `Vec<T>` inputs, interior synchronization, async pull methods,
and Rust-owned runtime integration. Generic, borrowed, provider-authoring, and
raw executor seams stay unannotated.

The existing `bindings/agentprism-ffi` is the legacy C/UniFFI artifact. Its
hand-rolled configuration and sequenced JSON delivery are not reused by this
SDK (`bindings/agentprism-ffi/src/lib.rs:190`,
`bindings/agentprism-ffi/src/lib.rs:290`,
`bindings/agentprism-ffi/src/lib.rs:689`).

**R3.** Authoritative `AgentEventEnvelope` and `AssistantEvent` delivery is
lossless async pull. BoltFFI's documented stream attribute requires an
`Arc<EventSubscription<T>>`, and the documented finite subscription buffer
drops new events when full while allowing the producer to continue. Therefore
`#[ffi_stream]`, `EventSubscription<AgentEventEnvelope>`, and
`EventSubscription<AssistantEvent>` are forbidden for authoritative delivery.
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#the-ffi_stream-attribute]
[https://www.boltffi.dev/docs/streaming.md | docs/boltffi-swift-bindings/docs-snapshot/streaming.md#buffer-capacity]

Ordinary exported Rust `async fn` methods map to target-language async methods,
and async `Result` maps to throwing async calls. This is the mechanism used by
`next_event`, `outcome`, and `cancel_and_wait`.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods]
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#error-handling]

### 1.3 In scope and deferred

Initial exported operations are:

- prompt text, prompt records, continue, and retry;
- steering, follow-up, run cancellation, reset, snapshot, wait-for-idle, and
  orderly shutdown;
- pull observation and reusable outcome settlement;
- distinct pull-plus-sink and sink-only prompt operations;
- direct `Models` streaming through `TokioAssistantStream`;
- provider-neutral native `Models` construction, persistent credentials,
  `check_auth`, and `login`;
- acknowledged `AgentEventSink` and the canonical auth interaction callbacks
  required by the OpenAI Codex device-code flow.

Initially unannotated are bare `Agent` streams, `ModelRuntime` trait-object
construction, provider traits, raw Tokio receivers/handles, scheduler streams,
generic `TypedTool<I, F>`, local/non-`Send` executor families, Swift-authored
tools, policies, storage backends, and provider extensions. Examples of those
Rust-only seams are `ModelRuntime`
(`crates/agentprism-ai/src/runtime.rs:87`), `TypedTool`
(`crates/agentprism-core/src/tools.rs:277`), and `HttpTransport`
(`crates/agentprism-ai/src/middleware.rs:275`).

## 2. Documentation evidence and Phase-0 unknowns

### 2.1 Documented facts used by this design

- Exported Rust classes are created by annotating an inherent impl; documented
  class methods include synchronous, async, throwing, constructor, and
  class-valued forms. A method may be deliberately omitted with `#[skip]`.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#defining-a-class]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#fallible-constructors]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#async-methods]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]
- Exported classes are checked as `Send + Sync` by default. This design does
  not use `single_threaded`; all mutable receiver state is behind interior
  synchronization.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#single-threaded-mode]
- The documentation says that deallocation of a target-language class drops
  its Rust struct. This makes Rust `Drop` a required last-resort cancellation
  boundary for established runs and streams.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management]
- Target-task cancellation cooperatively cancels the one exported Rust future;
  cleanup after an await is not guaranteed to execute. Cancelling a pending
  `next_event()` therefore does not mean “cancel the run.”
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#cancellation]
  [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation]
- BoltFFI supplies future polling, not a Tokio reactor. Tokio/reqwest work must
  execute under a runtime owned by this Rust library.
  [https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#runtime]
- Exported callback traits may be owned as `Arc<dyn Trait>`, may be `Send +
  Sync`, and may contain async methods which Rust awaits.
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#thread-safety]
  [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
- The documented integration uses a library `staticlib`, `build.rs` calling
  `boltffi::build::generate()`, and `boltffi check`. Root `boltffi.toml`
  selects package/crate identity and Apple module/layout/slices.
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]
  [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]
  [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#apple-configuration]
- The documented Apple pack operation builds configured device, simulator, and
  optional macOS slices and emits an XCFramework plus SwiftPM package.
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#apple-packaging]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
  [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#swiftpm-layouts]

### 2.2 Unresolved documentation questions

The snapshot manifest says no captured page identifies an exact BoltFFI
version (`docs/boltffi-swift-bindings/docs-snapshot/MANIFEST.md:14`). The owner
audit's 0.30.1 source-behavior statements are therefore not treated as
documented behavior. Every unresolved item below is a blocking Phase-0 probe;
none may be answered from model memory.

| Probe | Status and pages checked | Phase-0 proof |
|---|---|---|
| P0-01 exact version/source/CLI compatibility | **UNRESOLVED: not answered by the documentation.** All 19 pages; especially `installation.md#install-the-cli` and `#add-to-your-project`. [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#install-the-cli] | Pin crate, build crate, and CLI to `=0.30.1`; record `--version`, Cargo package IDs, and a golden generated probe. Any mismatch stops implementation. |
| P0-02 dependency/multi-crate annotation scanning | **UNRESOLVED: not answered by the documentation.** `installation.md#add-to-your-project`, `getting-started.md#write-your-code`, `configuration.md#package-identity`. [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project] [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code] [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity] | A disposable root depends on three annotated leaf crates; all leaf symbols must appear once in generated Swift/C and link from one static library. |
| P0-03 `cfg_attr` and optional annotation dependencies | **UNRESOLVED: not answered by the documentation.** `installation.md#add-to-your-project`, `getting-started.md#write-your-code`. [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#add-to-your-project] [https://www.boltffi.dev/docs/getting-started.md | docs/boltffi-swift-bindings/docs-snapshot/getting-started.md#write-your-code] | Generate with the feature on; compile all canonical crates with it off; prove no production symbol silently disappears. |
| P0-04 tuple newtypes and tuple-payload data enums | **UNRESOLVED: not answered by the documentation.** The records page demonstrates named structs and struct-style associated data, not every canonical tuple form. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#structs] [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums-with-associated-data] | Generate and Swift-round-trip every tuple/newtype family listed in Appendix A. |
| P0-05 tuple-payload and nested error values | **UNRESOLVED: not answered by the documentation.** `errors.md#enum-errors`, `#enums-with-payloads`. [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads] | Generate/catch `TokioAgentError::Agent(AgentError)` and every in-scope nested error with exact payload fidelity. |
| P0-06 owned classes and nested objects in async callbacks | **UNRESOLVED: not answered by the documentation.** `callbacks.md#async-methods`, `#ownership`, `#limitations`, `classes.md#methods-that-take-or-return-classes`. The docs cover callback ownership and async methods, but not an owned Rust class argument or one callback returning another callback object with boxed-self async methods. [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#ownership] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#limitations] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#methods-that-take-or-return-classes] | A Swift sink receives and invokes the owned `CancellationToken`. Generate the full canonical `AuthInteraction`/`RedirectReceiver` pair, including `create_redirect_receiver -> Box<dyn RedirectReceiver>`, owned `receive`, and exact auth values. Unsupported projection blocks Slice 6 for an owner-selected canonical reshape; it does not permit a device-only string protocol. |
| P0-07 `#[non_exhaustive]` data/error enums | **UNRESOLVED: not answered by the documentation.** `records.md#enums`, `errors.md#enum-errors`. [https://www.boltffi.dev/docs/records.md | docs/boltffi-swift-bindings/docs-snapshot/records.md#enums] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enum-errors] | Generate Swift switches/catches for current `AgentEvent`, `AssistantEvent`, `RequestStartErrorKind`, `TokioAgentError`, `AgentError`, `ControlError`, and `AuthError`, plus planned `TokioAssistantError`, `TokioRuntimeError`, and `NativeModelsError`. The source manifest fails if any further non-exhaustive enum enters the generated transitive closure without a probe case. |
| P0-08 complete owned value graph | **UNRESOLVED: not answered by the documentation.** `types.md#quick-reference`, `#whats-not-supported`, `custom-types.md#representation-types`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#whats-not-supported] [https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types] | Run Appendix A's path-specific matrix; no success may be generalized to another root. |
| P0-09 Swift 6 `Sendable` generation | **UNRESOLVED: not answered by the documentation.** The docs require Rust class/callback `Send + Sync` but do not promise Swift class or callback-protocol `Sendable`. `classes.md#thread-safety`, `#memory-management`, `callbacks.md#thread-safety`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management] [https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#thread-safety] | Inspect generated Swift under strict concurrency; apply the generator-owned resolution in F11 and compile it as Swift 6 with warnings as errors. |
| P0-10 completed output handoff and release window | **UNRESOLVED: not answered by the documentation.** `async-internals.md#generated-ffi-functions`, `#cancellation`, `classes.md#memory-management`. [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#generated-ffi-functions] [https://www.boltffi.dev/docs/async-internals.md | docs/boltffi-swift-bindings/docs-snapshot/async-internals.md#cancellation] [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management] | Instrument the disposable generated completion path and run F3's class-result tests plus a ready-event/cancellation race for both pull cursors. A ready event must reach Swift or remain the next Rust event; silent destruction is forbidden. |
| P0-11 `usize` error fields and constants | **UNRESOLVED: not answered by the documentation.** `types.md#quick-reference`, `#primitives`, `errors.md#enums-with-payloads`, `constants.md#supported-values`. [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference] [https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#primitives] [https://www.boltffi.dev/docs/errors.md | docs/boltffi-swift-bindings/docs-snapshot/errors.md#enums-with-payloads] [https://www.boltffi.dev/docs/constants.md | docs/boltffi-swift-bindings/docs-snapshot/constants.md#supported-values] | Round-trip `ControlError::QueueFull.capacity`; keep both `usize` capacity constants unannotated unless separately proven and intentionally added. |
| P0-12 generator-readable completeness metadata | **UNRESOLVED: not answered by the documentation.** `classes.md#skipping-methods`, `installation.md#verify-installation`. [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods] [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation] | Determine available generated artifacts, then implement F12's repository-owned source-to-generated contract checker. |

## 3. Resolution of all 16 implementation-audit findings

### F1 — current names and current tree

> “The branch is already stale relative to main.”

Resolved: this document uses `agentprism-ai`, `agentprism-core`,
`agentprism-runtime-tokio`, `agentprism-env`, `agentprism-session`,
`agentprism-harness`, `agentprism-providers-all`, provider leaves, and
`bindings/agentprism-ffi`. The workspace contains those current members
(`Cargo.toml:3`). No planned rename is part of this milestone. `pi-messages`
remains an API-family identifier, not a stale crate name.

### F2 — `accept_run` send-then-restore

> “accept_run has an uncovered cancellation race.”

Current `accept_run` checks `accepted.is_closed()`, publishes `idle = false`,
then sends acceptance without restoring idle on send failure
(`crates/agentprism-runtime-tokio/src/lib.rs:699`). Replace it exactly with the
send-then-restore rule:

```rust
fn accept_run(idle_tx: &watch::Sender<bool>, channels: &mut RunChannels) -> bool {
    let Some(accepted) = channels.accepted.take() else {
        return false;
    };

    let _ = idle_tx.send(false);
    if accepted.send(Ok(())).is_err() {
        let _ = idle_tx.send(true);
        return false;
    }
    true
}
```

There is no correctness-bearing `is_closed()` check. A `#[cfg(test)]` barrier
between `idle_tx.send(false)` and `accepted.send(Ok(()))` deterministically
drops `accepted_rx`, then proves idle returns to true, the unaccepted stream is
dropped, and the next run is accepted. This is acceptance test 20.

### F3 — established-output drop policy for both classes

> “The establishment guard does not cover the complete foreign handoff race.”

Resolved by selecting the first audit option: **dropping an established active
`TokioAgentRun` or `TokioAssistantStream` closes observation and cancels the
same work token retained by its producer**.

Both classes implement synchronous `Drop`. `Drop` cancels the token, signals
observation closure, and closes the receiver through `Mutex::get_mut`; it never
blocks and never waits for settlement. The actor/producer owns a separate
runtime lease and finishes cancellation asynchronously. Explicit `outcome`,
`cancel_and_outcome`, and `cancel_and_wait` remain the APIs for callers that
need to await settlement. Once terminal completion is cached, `Drop` is a
no-op apart from releasing fields.

The documentation states only that target-language class deallocation drops
the Rust struct; it does not describe the post-ready/pre-retain interval.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#memory-management]
**Audit/snapshot status:** the audit reports that generated cleanup drops an
unclaimed ready class result; the snapshot neither confirms nor contradicts
that exact interval. Therefore the exact generated handoff is P0-10, not a
memory-based assertion.
A test-only hook in the disposable generated completion path pauses after the
Rust future is ready and before Swift stores the returned class. Releasing that
output must run `Drop`, cancel the token, settle the actor/producer, and release
the lease. Separate deterministic tests cover `TokioAgentRun` and
`TokioAssistantStream` (tests 21 and 22). If the hook cannot locate and prove
that interval, Phase 0 blocks production annotation.

The same generated path carries owned event results. P0-10 also races
cancellation after `Ready(Some(event))` and before Swift retention. If stock
0.30.1 can discard that value, the pinned generator patch must make completion
win once the Rust future is ready; cancellation may win only while the Rust
future is still pending. Test 31 proves the next cursor call cannot observe a
gap. This rule is generator-owned and introduces no acknowledgement envelope.

### F4 — assistant drop releases the runtime lease

> “TokioAssistantStream can retain the runtime indefinitely when dropped.”

`TokioAssistantStream::drop` performs the F3 cancellation/closure. Its producer
selects among provider progress, cancellation, and observation closure. When
observation is closed and cancellation is set, it drops a pending provider
stream without waiting for another provider event, publishes an internally
abandoned terminal state, and releases `RuntimeLease` in the producer task's
RAII epilogue. Active observation still requires a canonical terminal
`AssistantEvent`; only an explicitly closed/dropped observation may use the
internal-abandonment path.

Test 23 uses a provider stream that remains pending forever, drops the
established class without calling `cancel_and_wait`, and proves token
cancellation, producer exit, lease count zero, and runtime-thread teardown.

### F5 — sink method names preserve semantics

> “Sink-only semantics silently change an existing API.”

The established name keeps its established behavior:

```rust
pub async fn prompt_text_with_sink(
    &self,
    prompt: PromptText,
    sink: Arc<dyn AgentEventSink>,
) -> Result<TokioAgentRun, TokioAgentError>; // pull + sink

pub async fn prompt_text_sink_only(
    &self,
    prompt: PromptText,
    sink: Arc<dyn AgentEventSink>,
) -> Result<TokioAgentRun, TokioAgentError>; // no observation sender
```

The current `prompt_text_with_sink` returns a normal run
(`crates/agentprism-runtime-tokio/src/lib.rs:217`); that contract is not
silently changed. `RunChannels.events` becomes optional. The first method
installs `Some(sender)`; the second installs `None` and returns a run in
`ObservationState::NeverInstalled` so it can cancel and await outcome without a
hidden drainer. Test 24 proves both behaviors and test 6 proves sink-only
delivery beyond capacity.

### F6 — producer validation versus consumer delivery

> “EOF rules contradict sink-only and abandoned-observation runs.”

The implementation tracks two independent facts:

```rust
enum ProducerTerminal<R, E> {
    Pending,
    Validated(R),
    ProtocolError(E),
}

enum ObservationState<R> {
    Active { delivered_terminal: Option<R> },
    ClosedByOutcome,
    NeverInstalled,
}
```

The concrete storage may combine these with the completion watch value, but it
must preserve the distinction.

- With `Active`, `next_event()` returns `Ok(None)` only after the pull cursor
  delivered exactly one matching `RunFinished`/assistant terminal, producer
  validation agrees, completion is cached, and all required sinks settled.
- With `ClosedByOutcome`, `next_event()` waits for cached producer validation
  and then returns `Ok(None)`; consumer delivery is intentionally irrelevant.
- With `NeverInstalled`, `next_event()` likewise waits for internally validated
  completion and returns `Ok(None)`; no pull terminal was promised.
- Raw producer EOF while active without a delivered terminal returns
  `MissingRunFinished` or `MissingTerminalEvent`, never `Ok(None)`.
- `SnapshotInvariant`, actor/producer loss, or a mismatch between delivered and
  cached terminal remains an error. Expected provider failure and cancellation
  remain in-band `RunOutcome`/`AssistantEvent` values
  (`crates/agentprism-core/src/events.rs:51`,
  `crates/agentprism-ai/src/runtime.rs:42`).

Observation closure uses a separate cancellation-safe wake signal so
`outcome()` can wake a blocked pull before acquiring and closing its receiver.
Test 25 covers active, outcome-closed, and never-installed EOF states.

### F7 — concurrent pulls are rejected

> “Concurrent pull serialization does not guarantee consumer ordering.”

`TokioAgentRun` is a single-consumer cursor. Each `next_event()` first acquires
an atomic `PollPermit` using compare/exchange. Failure returns
`TokioAgentError::ConcurrentEventPoll`; the permit's `Drop` clears the bit even
when Swift cancels the pending future. Receiver access remains behind a Tokio
mutex for memory safety, not to promise multi-consumer ordering.

`TokioAssistantStream` applies the same rule with
`TokioAssistantError::ConcurrentEventPoll`. Test 26 launches two simultaneous
pulls, requires exactly one rejection, then continues on the surviving cursor
and proves sequence completeness and uniqueness.

### F8 — no envelope sequence gap after `SnapshotInvariant`

> “Envelope sequencing has an unaddressed invariant-error gap.”

Resolution selected: **construct and dispatch one authoritative envelope for
the core-produced event before reporting a Tokio mirror invariant failure**.
This preserves the existing low-level `AgentEvent` stream while making the
Tokio envelope sequence complete. Core allocation was not selected because it
would reshape the deliberately unexported borrowed `AgentEvent` stream merely
to serve this boundary.

The actor algorithm is:

1. Once `RunStarted` establishes `active_run_id`, read the authoritative
   sequence from the pre-apply `AgentSnapshot.next_sequence` and construct one
   `AgentEventEnvelope` for the event. `AgentSnapshot` identifies that field as
   the next envelope sequence (`crates/agentprism-core/src/state.rs:185`).
2. Apply the event to cloned tentative snapshot/assembler state. Current code
   mutates `next_sequence` before later validation
   (`crates/agentprism-runtime-tokio/src/lib.rs:860`); the transactional clone
   prevents partially published mirror state.
3. On success, publish the tentative snapshot, then fan out the one envelope.
4. On `SnapshotInvariant`, fan out that same envelope to active observation and
   every sink and await the sink barriers. Return a mirror-failure drive result
   carrying the rejected sequence; do not publish the partially applied
   tentative snapshot.
5. Once the borrowed core stream has been dropped, the actor publishes a fresh
   canonical `agent.snapshot()`, verifies that its `next_sequence` equals the
   rejected envelope sequence plus one, then caches `SnapshotInvariant` and
   settles idle. A mismatch poisons the actor. Thus the next accepted run uses
   the following core sequence, and no consumer or binding layer repairs a
   number. The current finish path already refreshes from `agent.snapshot()`
   after the borrowed stream returns
   (`crates/agentprism-runtime-tokio/src/lib.rs:727`).
6. A protocol failure before `RunStarted` has established a run identity
   permanently poisons that actor: no later run is accepted, so no later
   visible sequence can conceal a gap. Orderly shutdown remains available.

This keeps the current reducer-before-listener order for successful events and
defines the exceptional path explicitly. Test 27 forces an assistant
snapshot-mirror invariant after `RunStarted`, observes the rejected event's
envelope and `SnapshotInvariant`, starts a second run, and proves a globally
consecutive sequence. It also tests the pre-`RunStarted` poison rule.

### F9 — one canonical BoltFFI root library

> “There is no defined BoltFFI root library.”

Add the ordinary Rust native assembly crate `crates/agentprism-native`. It is
not a binding facade: it owns provider-neutral native construction and
re-exports canonical consumer types; it defines no duplicate event, record,
command, or error hierarchy.

```text
agentprism-native  [lib + staticlib; build.rs; selected source crate]
├── agentprism-runtime-tokio
│   ├── agentprism-core
│   │   └── agentprism-ai
│   ├── agentprism-env
│   └── agentprism-ai
├── agentprism-providers-all
│   ├── provider leaves
│   ├── agentprism-provider-common
│   └── agentprism-ai
├── agentprism-transport-reqwest
│   ├── agentprism-runtime-tokio
│   └── agentprism-ai
├── agentprism-core
└── agentprism-ai
```

Only `agentprism-native` owns `[lib] crate-type = ["lib", "staticlib"]`, the
exact BoltFFI build dependency, and `build.rs`. Canonical crates containing
inline annotations may add the exact normal dependency behind the Phase-0-
proven feature; they do not add `staticlib`, `build.rs`, or packaging config.
The stock dependency contract is:

```toml
# agentprism-native/Cargo.toml
[dependencies]
boltffi = "=0.30.1"

[build-dependencies]
boltffi = "=0.30.1"

# each lower canonical crate that owns annotations
[features]
boltffi = ["dep:boltffi"]

[dependencies.boltffi]
version = "=0.30.1"
optional = true
```

The root build script is exactly:

```rust
fn main() {
    boltffi::build::generate();
}
```

That integration shape is documented; P0-01 determines whether the exact pin
needs the F11 source patch.
[https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#create-buildrs]

`agentprism-native` enables those lower `boltffi` features on its dependency
edges; their default Rust builds do not. If F11 requires a patch, a single
workspace `[patch.crates-io]` redirects every `boltffi` dependency to the same
vendored 0.30.1-derived source recorded by upstream commit, patch SHA-256, and
normalized tree SHA-256 in the artifact manifest. The matching CLI is installed
from that vendored workspace. Mixing stock support code with a patched
generator is a hard error.

Workspace-root `boltffi.toml` is owned by this root and fixes:

```toml
[package]
name = "agent_prism"
crate = "agentprism_native"

[targets.apple]
output = "dist/apple"
deployment_target = "15.0"
ios_architectures = ["arm64"]
simulator_architectures = ["arm64", "x86_64"]
include_macos = true
macos_architectures = ["arm64", "x86_64"]

[targets.apple.swift]
module_name = "AgentPrism"

[targets.apple.spm]
layout = "ffi-only"
```

It selects Cargo crate `agentprism_native`, package/module `AgentPrism`, and
the Apple output. Provider/runtime symbols are reachable
through this downward-only dependency graph; no lower crate depends on
`agentprism-native`, so there is no cycle. `bindings/agentprism-ffi` remains a
separate legacy artifact and is not linked into the new XCFramework.

Whether the generator discovers inline annotations in dependencies is P0-02.
The root choice and dependency direction are fixed even if that probe blocks.
The documentation's package/crate selection behavior is cited here; it does
not answer dependency scanning.
[https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#package-identity]

### F10 — repository-owned `xtask package-apple`

> “The required xtask package-apple pipeline is missing.”

Add workspace member `xtask` and repository-owned `.cargo/config.toml` alias
`xtask = "run --quiet --locked --package xtask --"`. This makes
`cargo xtask package-apple` the only release packaging entrypoint. It
never resolves `boltffi` from `PATH`; the stock bootstrap command is exactly:

```bash
cargo install --root target/boltffi-tools --version 0.30.1 --locked boltffi_cli
```

All later invocations use
`target/boltffi-tools/bin/boltffi`. It performs, in order:

1. Require `Cargo.lock`; verify the `boltffi` normal/build packages and
   `boltffi_cli` have base version `0.30.1`. For stock 0.30.1, install the CLI
   with `--version 0.30.1 --locked` into a repository-local tool directory. If
   F11 requires a generator fix, pin the 0.30.1 source plus the recorded patch
   by immutable tree hash for both crates and install the CLI from that same
   source. P0-01 must verify the exact package IDs, source hash, and golden
   output. The compatibility fingerprint consists of: one and only one
   `boltffi` 0.30.1 Cargo source ID across normal/build dependency graphs; the
   local CLI's exact version/source hash; normalized Swift, C header, and Rust
   symbol goldens for an async class/callback probe; and a linked Swift call to
   that probe's version sentinel. Any mismatch stops before production
   generation.
2. Run the P0-01 generator/source compatibility fingerprint and `boltffi
   check`. The documentation defines `check` as the installation verification
   command.
   [https://www.boltffi.dev/docs/installation.md | docs/boltffi-swift-bindings/docs-snapshot/installation.md#verify-installation]
3. Run pinned `boltffi generate swift` into a clean staging directory; run the
   source-side F12 inventory, the no-authoritative-`EventSubscription` check,
   and a second generation whose normalized output must diff cleanly.
4. Run pinned `boltffi build apple --release` and require release slices
   `aarch64-apple-ios`,
   `aarch64-apple-ios-sim`, `x86_64-apple-ios`,
   `aarch64-apple-darwin`, and `x86_64-apple-darwin`. `Cargo.lock` must be
   unchanged before and after the command.
   `boltffi.toml` explicitly enables iOS device arm64, simulator arm64/x86_64,
   and macOS arm64/x86_64. Those configurable slice families are documented.
   [https://www.boltffi.dev/docs/configuration.md | docs/boltffi-swift-bindings/docs-snapshot/configuration.md#apple-slice-selection]
   The separate generate/build/pack commands are the documented staged
   workflow.
   [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#step-by-step-workflow]
5. Invoke the pinned `boltffi pack apple --release --no-build` against those
   staged slices. The documented operation emits the XCFramework, C header,
   generated Swift, and SwiftPM package.
   [https://www.boltffi.dev/docs/packaging.md | docs/boltffi-swift-bindings/docs-snapshot/packaging.md#apple-packaging]
6. Run the complete F12 check against generated Swift and the packed C header,
   then validate names and contents: package/product/module `AgentPrism`,
   `AgentPrism.xcframework`, binary target `AgentPrismFFI`, root library
   `libagentprism_native.a`, the three platform slices (iOS device, iOS
   Simulator, macOS) containing all five configured target architectures, one
   generated Swift target, no legacy `pi_` symbols in the new header, and no
   handwritten production Swift wrapper.
7. Run `swift test --package-path bindings/boltffi-swift-tests` under Swift 6
   strict concurrency with warnings as errors. That test-only package has a
   relative dependency on freshly generated `dist/apple`; it contains XCTest,
   not a production wrapper. Then run `swift build --package-path dist/apple`
   for macOS and `xcodebuild` the generated `AgentPrism` package scheme for a
   generic iOS Simulator destination. P0-09 records the exact Swift compiler
   flags and generated scheme spelling before this command is frozen.
8. Emit `dist/apple/artifact-manifest.json` containing exact tool versions,
   Cargo package IDs, targets, normalized file hashes, exported symbol set, and
   generated-surface manifest hash.
9. A macOS CI reproducibility job pins and records `rustc`, Cargo, Xcode, Swift,
   SDK, and deployment-target versions; creates a fresh checkout; invokes the
   one command with no untracked inputs; and compares the normalized manifest
   and generated sources to a second fresh-checkout run. Network caches may be
   prewarmed, but neither run may read files outside the checkout/tool cache
   declared in the manifest. Absolute checkout paths, timestamps, signing
   metadata, simulator UDIDs, and archive ordering are normalized by named
   rules recorded in the manifest; public symbols and file contents are never
   normalized away.

The exact version is an owner-selected implementation pin, not a claim that
the snapshot documents 0.30.1; the snapshot explicitly does not. P0-01 is the
blocking empirical verification. P0-01 also freezes every derived artifact
name; the pipeline fails if the pinned generator derives a different name
instead of silently renaming the contract.

### F11 — Swift 6 `Sendable` is generator-owned

> “Swift class Sendable is unaddressed.”

The Rust export remains subject to the documented default `Send + Sync`
compile-time check.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]
The documentation does not state that generated Swift classes conform to
`Sendable`, so this is P0-09.

**Audit/snapshot status:** the audit's 0.30.1 source inspection reports a plain
`public final class` with no `Sendable` conformance. The snapshot neither
confirms nor contradicts that source behavior. Slice planning therefore
assumes the patch is required, while P0-09 must still reproduce the exact
generated output before the source is changed.

If stock 0.30.1 does not generate an adequate conformance, the repository pins
the 0.30.1 source plus a minimal BoltFFI generator patch by immutable tree hash
for both generator and Rust support crate. The patch emits
`@unchecked Sendable` conformance only for Rust classes exported under the
default `Send + Sync` mode. It must not emit that conformance for
`single_threaded` classes. For callback traits carrying Rust `Send` (including
the shared `Send + Sync` case), it also emits a Swift protocol refinement to
`Sendable`; callback traits without `Send` do not receive it. Rust ownership
still distinguishes one-owner `Box` from shared `Arc`. Both mappings are P0-09
goldens. This is generated source owned by the generator, not a required
handwritten Swift extension or wrapper. `xtask` verifies the patched generator
and Rust crate share the P0-01 compatibility fingerprint.

Tests compile task transfer and simultaneous calls for `TokioRuntimeOwner`,
`NativeModelsFactory`, `Models`, `TokioAgentFactory`, `TokioModelClient`,
`TokioAgentHandle`, `TokioAgentRun`, `TokioAssistantStream`, and
`CancellationToken`, plus Sendable Swift implementations of `AgentEventSink`,
`AuthInteraction`, and `RedirectReceiver`, using Swift 6 strict concurrency
with warnings as errors. Test 29 is blocking.

### F12 — generated-surface completeness gate

> “There is no generated-surface completeness gate.”

`xtask boltffi-surface` parses canonical Rust sources and records every
BoltFFI-annotated type, trait, impl, constant, constructor, and public method in
`boltffi-surface.toml`. For every public item inside an annotated impl there
must be exactly one of:

- a generated Swift/C contract entry; or
- BoltFFI's documented `#[skip]` plus a matching manifest entry containing a
  nonempty Rust-consumer reason and an owner/type/method identity.
  [https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#skipping-methods]

The checker fails for an annotated source item missing from output, an output
item without source ownership, a new public impl method without a decision, a
`#[skip]` without a reason, a stale reason, duplicate generated names, or a
manifest change not committed with generated-contract changes. Test-only
fixtures use a separate manifest and may not appear in the production one.

The docs do not promise a machine-readable generator manifest; P0-12 selects
the exact generated artifacts to inspect. If none is sufficient, the checker
parses the generated Swift and C header and compiles a generated use-site file.
It does not create a facade.

### F13 — Phase 0 before canonical implementation

> “Implementation order is too risky.”

Phase 0 is disposable and proves P0-01 through P0-12 before broad canonical
changes. It covers multi-crate scanning, `cfg_attr`, tuples/newtypes,
tuple/nested errors, owned class callback arguments, non-exhaustive enums, the
complete owned value graph, post-ready/pre-retain cleanup, generator surface
metadata, and Swift 6 strict concurrency. Its fixtures may use canonical leaf
types, but the probe target is never shipped and never becomes a facade.

After Phase 0, the eight production slices in section 8 are individually
gated. No actor/runtime/provider rewrite is begun on an unproven projection.

### F14 — provider-neutral native HTTP transport

> “OpenAiModelsFactory puts transport in the wrong layer.”

Add `crates/agentprism-transport-reqwest`. Its concrete
`ReqwestTransport` implements the provider-neutral `HttpTransport` contract
(`crates/agentprism-ai/src/middleware.rs:274`) and contains native Tokio/reqwest
configuration, response-body streaming, cancellation, TLS, timeout, and public
error sanitization. It depends on `agentprism-ai` and
`agentprism-runtime-tokio`, never on a provider; runtime-tokio does not depend
back on the transport crate, so the graph remains acyclic.

`ReqwestTransport::new` takes an internal cloneable executor capability from
`TokioRuntimeOwner`, not a raw public Tokio handle. Runtime-tokio exposes that
capability as an ordinary Rust-only `TokioExecutor`; it is absent from the
generated surface. Each `execute` schedules the
reqwest request and response-body pump under runtime leases and returns an
executor-neutral future/stream bridge. Its bounded body channel awaits every
send and selects cancellation/receiver closure while the reqwest body is
pending; it never uses `try_send` or a drop-on-full queue. Consequently a
Bolt-polled `Models::login` or auth refresh never requires an ambient Tokio context, while
provider leaves still know only `HttpTransport`. Dropping a `Models` value
releases its transport owner reference; pending request/body tasks retain their
own leases until cancellation settles.

Provider leaves remain transport-neutral and continue receiving
`Arc<dyn HttpTransport>`. That is already the current `ProviderInputs` shape
(`providers/agentprism-provider-common/src/registration.rs:20`,
`providers/agentprism-provider-common/src/registration.rs:22`) and the
OpenAI Codex leaf's direct input
(`providers/agentprism-openai-codex/src/lib.rs:47`).
`agentprism-providers-all` composes the same transport into provider
registrations (`providers/agentprism-providers-all/src/lib.rs:270`).

### F15 — provider-neutral native `Models` factory and real auth flow

> “The factory bypasses the intended auth/control-plane flow.”

`agentprism-native` owns `NativeModelsFactory`, a legitimate native Rust
assembly API. It returns canonical `Models`; it does not return an OpenAI-only
wrapper. Configuration includes a persistent credential-file path, native
transport options, explicit provider selection, and host environment values.
The initial portable provider set includes at least `openai` and
`openai-codex`; API-key OpenAI is not the sole construction path.

Construction uses:

- `ReqwestTransport` from F14;
- `FileCredentialStore`, whose current implementation is a persistent
  provider-keyed store with serialized leases
  (`crates/agentprism-ai/src/file_credentials.rs:24`);
- current provider registrations from `agentprism-providers-all`, including
  OpenAI Codex (`providers/agentprism-providers-all/src/lib.rs:338`);
- `ModelsBuilder::credential_store`, provider registration, and build
  (`crates/agentprism-ai/src/models.rs:1476`,
  `crates/agentprism-ai/src/models.rs:1501`,
  `crates/agentprism-ai/src/models.rs:1541`).

The returned canonical `Models` preserves synchronous catalog access,
`check_auth`, `login`, and model execution
(`crates/agentprism-ai/src/models.rs:132`,
`crates/agentprism-ai/src/models.rs:167`,
`crates/agentprism-ai/src/models.rs:313`,
`crates/agentprism-ai/src/models.rs:762`). `Models::login` persists the
provider result through the configured credential lease
(`crates/agentprism-ai/src/models.rs:334`). The OpenAI Codex provider registers
its OAuth resolver over the shared transport
(`providers/agentprism-openai-codex/src/lib.rs:94`); its login includes a
device-code branch (`providers/agentprism-openai-codex/src/oauth.rs:97`).

The concrete native construction API is:

```rust
pub struct NativeEnvironmentEntry {
    pub name: String,
    pub value: String,
}

pub struct NativeModelsConfig {
    pub credential_file: String,
    pub providers: Vec<ProviderId>,
    pub environment: Vec<NativeEnvironmentEntry>,
    pub request_timeout_ms: u64,
}

#[export]
impl NativeModelsFactory {
    pub fn new(
        runtime: &TokioRuntimeOwner,
        config: NativeModelsConfig,
    ) -> Result<Self, NativeModelsError>;
    pub fn build(&self) -> Result<Models, NativeModelsError>;
}
```

An empty `providers` vector selects the portable set, including `openai` and
`openai-codex`; a nonempty vector preserves caller order and fails with the
named provider when native construction for it is unavailable. The factory
rejects duplicate environment names and converts the concrete vector to the
provider layer's `BTreeMap`. A zero request timeout is invalid. The credential
path must be nonempty and is opened as a `FileCredentialStore` at construction,
so a malformed or unwritable location fails before `Models` is published.

To keep leaf selection in the aggregation layer, `agentprism-providers-all`
adds the ordinary Rust API
`selected_http_providers(ids: &[ProviderId], inputs: ProviderInputs)`. It
dispatches each supported ID to its leaf constructor, preserves requested
order, rejects duplicates/unknown IDs, and returns a named
`MissingNativeCapability` for providers such as Bedrock that require more than
the provider-neutral HTTP/environment inputs. It never builds all providers
and discards the unselected registrations. `NativeModelsFactory` consumes only
this aggregate API; it does not grow provider-specific branches.

The generated ordinary-consumer surface includes concrete `Models` methods for
model lookup, `check_auth`, `login`, and logout, plus the canonical
`AuthInteraction` callback protocol required by login. Callback projection is
subject to P0-06; no substitute string command protocol is allowed.

Today `Models::{check_auth,login,logout}` return `SendBoxFuture` rather than
being `async fn` (`crates/agentprism-ai/src/models.rs:167`,
`crates/agentprism-ai/src/models.rs:313`,
`crates/agentprism-ai/src/models.rs:350`), and `AuthInteraction` likewise uses
boxed-future methods (`crates/agentprism-ai/src/auth.rs:1085`). Slice 6 moves
the three canonical `Models` methods to a focused exported impl and changes
them to true `async fn`s. It rewrites the Send `AuthInteraction` and
`RedirectReceiver` source contracts as `#[async_trait]` async methods while
retaining their object-safe behavior. This is the documented shape for async
callback traits; P0-06 must prove this repository's full signatures before the
rewrite lands.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]

The existing boxed-future bodies remain straightforward implementation bodies
for the async methods; the boundary may not pretend a boxed return is a
documented BoltFFI async export. The local/non-`Send` family remains
unannotated and unchanged. Catalog projection adds an owned
`Vec<ModelDescriptor>` convenience because current `Models::models()` returns
`Arc<[ModelDescriptor]>`
(`crates/agentprism-ai/src/models.rs:132`). These are ordinary Rust API
improvements, not binding commands.

Test 17 is a captured, no-live-network acceptance path:

1. Use the same internal `NativeModelsFactory::build_with_transport` assembly
   function with a Rust `CapturingHttpTransport`; the public `build` supplies
   `ReqwestTransport`. The injection overload is ordinary Rust-only.
2. Exercise both a fixture-backed persisted OAuth credential and a complete
   captured device-code login, verifying the credential is committed to a
   temporary `FileCredentialStore` and `check_auth` succeeds. The generated
   Swift test has no ambient Tokio runtime; all transport progress is through
   the supplied `TokioRuntimeOwner`.
3. Resolve provider `openai-codex`, model `gpt-5.6-sol` from the pinned current
   catalog (`providers/agentprism-openai-codex/data/models.json:2`), set
   `ReasoningLevel::Xhigh`, and start the direct model stream.
4. Assert the captured request selects the Codex Responses family, exact model,
   and xhigh reasoning, then feed captured SSE through to a terminal
   `AssistantEvent`. No socket or live provider is used.

### F16 — runtime owner and factories are separate

> “TokioRuntimeOwner conflates executor and application assembly.”

The combined design is rejected. Use three concrete classes:

```rust
pub struct TokioRuntimeOwner { /* executor, supervisor, lease registry only */ }

pub struct TokioModelClient {
    runtime: TokioRuntimeOwner,
    models: Models,
    event_capacity: usize,
}

pub struct TokioAgentFactory {
    model_client: TokioModelClient,
    tools: ToolRegistry,
    command_capacity: usize,
}
```

`TokioRuntimeOwner` starts/retains the native runtime and supervises leased
tasks; it stores no `Models` and no `ToolRegistry`. `TokioModelClient` owns the
canonical Models clone and starts direct assistant streams under runtime
leases. `TokioAgentFactory` owns the tool registry and uses the model client's
`Models` as the narrow `ModelRuntime` injected into `Agent::new`
(`crates/agentprism-core/src/run.rs:217`). `TokioRuntimeOwner` is a cloneable
class whose clones share an internal `Arc<RuntimeSupervisor>`; the public type
is stored directly rather than nested behind another public `Arc`. `Models`
remains an independently usable control plane; the runtime owner never
performs auth or catalog operations.

## 4. Canonical Rust contracts

### 4.1 `TokioAgentRun`

Current `TokioAgentRun` contains a mutable mpsc receiver and consuming oneshot
completion (`crates/agentprism-runtime-tokio/src/lib.rs:126`), and its methods
are `next_event(&mut self)` and `outcome(self)`
(`crates/agentprism-runtime-tokio/src/lib.rs:131`). Replace that shape with:

```rust
pub struct TokioAgentRun {
    events: tokio::sync::Mutex<Option<mpsc::Receiver<AgentEventEnvelope>>>,
    completion: watch::Receiver<Option<Result<RunOutcome, TokioAgentError>>>,
    cancellation: CancellationToken,
    observation_closed: CancellationToken,
    poll_in_flight: AtomicBool,
    observation: Mutex<ObservationState<RunOutcome>>,
}

#[export]
impl TokioAgentRun {
    pub async fn next_event(
        &self,
    ) -> Result<Option<AgentEventEnvelope>, TokioAgentError>;

    pub async fn outcome(&self) -> Result<RunOutcome, TokioAgentError>;

    pub fn cancel(&self);

    pub async fn cancel_and_outcome(
        &self,
    ) -> Result<RunOutcome, TokioAgentError>;
}
```

`TokioAgentError` becomes cloneable and adds
`ConcurrentEventPoll`, `Runtime(TokioRuntimeError)`, and
`ActorPoisoned { message }`. Operational
`RunOutcome::{Failed,Cancelled}` stays in-band
(`crates/agentprism-core/src/events.rs:61`).

`outcome()` atomically changes Active observation to `ClosedByOutcome`, signals
`observation_closed`, acquires the receiver after a pending pull wakes,
closes/discards it, and awaits a reusable cached completion. Cancelling one
Swift `outcome()` await does not consume completion. `cancel_and_outcome()`
first cancels the run token and then performs the same close-and-wait path.

An unannotated impl may expose raw receiver access to Rust callers if still
needed. It must not be in the annotated impl; deliberate omission uses F12's
`#[skip]` plus reason discipline.

### 4.2 Establishment and completion

`request_run` creates the cancellation token before command submission and
puts the same token in `RunChannels`, `RunEstablishmentGuard`, and the eventual
`TokioAgentRun`. The guard is armed from command submission until a synchronous
handoff constructs the run. Dropping it cancels the token and closes
observation. There is no await between disarming it and returning
`Ok(TokioAgentRun)`.

This guard covers cancellation while the Rust establishment future is pending;
F3's class `Drop` covers an already-ready unclaimed output. The current request
and acceptance sites are `crates/agentprism-runtime-tokio/src/lib.rs:394` and
`crates/agentprism-runtime-tokio/src/lib.rs:699`.

Completion is published only after terminal producer validation, envelope
dispatch, registered sinks in registration order, the run-scoped sink, final
snapshot publication, and idle publication. Current dispatch sends observation
before awaiting registered and run-scoped sinks
(`crates/agentprism-runtime-tokio/src/lib.rs:839`). That order is retained.

### 4.3 Sink contract

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

The actor awaits every sink. This is an acknowledgement barrier, not an
observational subscription. The documented async callback mapping says Rust
awaits the target implementation.
[https://www.boltffi.dev/docs/callbacks.md | docs/boltffi-swift-bindings/docs-snapshot/callbacks.md#async-methods]
The owned class callback argument remains P0-06.

Sinks may synchronously cancel through the supplied token, call
`cancel_now`, or read `latest_snapshot`. They must not await mailbox methods
which are queued behind their own acknowledgement. Current re-entrant methods
are at `crates/agentprism-ai/src/cancellation.rs:62`,
`crates/agentprism-runtime-tokio/src/lib.rs:301`, and
`crates/agentprism-runtime-tokio/src/lib.rs:364`.

### 4.4 `TokioAssistantStream`

```rust
pub struct TokioAssistantStream {
    events: tokio::sync::Mutex<Option<mpsc::Receiver<AssistantEvent>>>,
    completion: watch::Receiver<Option<Result<AssistantTerminal, TokioAssistantError>>>,
    cancellation: CancellationToken,
    observation_closed: CancellationToken,
    poll_in_flight: AtomicBool,
    observation: Mutex<ObservationState<AssistantTerminal>>,
}

#[export]
impl TokioAssistantStream {
    pub async fn next_event(
        &self,
    ) -> Result<Option<AssistantEvent>, TokioAssistantError>;

    pub fn cancel(&self);

    pub async fn cancel_and_wait(&self) -> Result<(), TokioAssistantError>;
}
```

`TokioModelClient::stream_model(request)` establishes the canonical
`Models::stream_simple` on the owned runtime
(`crates/agentprism-ai/src/models.rs:762`), then hands back the pull object.
The producer owns a `RuntimeLease`, awaits bounded sends, and distinguishes the
three current terminal variants from raw EOF
(`crates/agentprism-ai/src/streaming.rs:521`,
`crates/agentprism-ai/src/streaming.rs:528`,
`crates/agentprism-ai/src/streaming.rs:534`,
`crates/agentprism-ai/src/streaming.rs:1934`).

Cancelling a pending pull cancels only that pull future. `cancel()` cancels the
model token and permits a conforming producer to deliver the terminal lifecycle
to active observation. `cancel_and_wait()` abandons observation and waits for
internal settlement. Drop is the non-waiting abandonment path defined in F3
and F4.

### 4.5 Runtime and factory methods

```rust
#[export]
impl TokioRuntimeOwner {
    pub fn new() -> Result<Self, TokioRuntimeError>;
    pub async fn shutdown(&self) -> Result<(), TokioRuntimeError>;
}

#[export]
impl TokioModelClient {
    pub fn new(
        runtime: &TokioRuntimeOwner,
        models: &Models,
    ) -> Self;

    pub async fn stream_model(
        &self,
        request: ModelRequest,
    ) -> Result<TokioAssistantStream, RequestStartError>;
}

#[export]
impl TokioAgentFactory {
    pub fn without_tools(model_client: &TokioModelClient) -> Self;

    pub fn spawn_agent(
        &self,
        state: AgentState,
    ) -> Result<TokioAgentHandle, TokioAgentError>;
}
```

The ordinary Rust-only constructor
`TokioAgentFactory::new(model_client, tools: ToolRegistry)` remains in an
unannotated impl and stores the cloneable canonical registry
(`crates/agentprism-core/src/tools.rs:490`). Only `without_tools` is generated
in this milestone because Swift-authored tools and `Tool` callback projection
are explicitly deferred. This keeps `TokioAgentFactory` truthfully responsible
for `Models` plus `ToolRegistry` plus spawning without pulling a separate
callback-authoring milestone into the package.

The Rust-only runtime impl exposes handles and lease operations. Exported
classes use `&self` plus interior synchronization and must pass the documented
default `Send + Sync` check.
[https://www.boltffi.dev/docs/classes.md | docs/boltffi-swift-bindings/docs-snapshot/classes.md#thread-safety]

`TokioRuntimeOwner::new` starts a named Rust supervisor thread. That thread
constructs the Tokio runtime, publishes only a Tokio `Handle` through the
internal supervisor, and keeps ownership of the `Runtime` until shutdown. The
supervisor tracks two independent atomics: public owner references and active
`RuntimeLease`s. Lease acquisition increments before spawn, rechecks the
closing bit, and rolls back if shutdown won the race. Every spawned future is
wrapped so unwind, cancellation, and normal return all drop its lease and
notify the supervisor. No public method uses `Handle::try_current`.

Explicit `shutdown()` atomically closes lease acquisition, signals every
supervised cancellation token, waits for actor/producer exit and a zero lease
count, then awaits a stopped signal sent after the supervisor thread has
dropped the Tokio runtime. It is idempotent and reusable by concurrent callers.
The last public owner reference requests the same shutdown nonblocking; it
never joins from `Drop`. New agent or model-stream establishment after closing
returns `TokioRuntimeError::ShuttingDown` before work is accepted.

Each actor and direct producer captures a lease before spawning and releases it
only after its task exits. `TokioAgentHandle::shutdown()` waits for an
actor-done signal after mailbox acknowledgement; current shutdown sends its
acknowledgement at the actor boundary before return
(`crates/agentprism-runtime-tokio/src/lib.rs:384`,
`crates/agentprism-runtime-tokio/src/lib.rs:690`). Runtime shutdown waits for
all leases. Dropping the last `TokioRuntimeOwner`/client/factory reference marks
shutdown requested; a live `RuntimeLease` retains only the supervisor until its
task epilogue, so owner release cannot tear down a runtime under live work.

### 4.6 `TokioAgentHandle` exported contract

The generated handle impl is the existing actor API with the F5 addition and
the concrete-record change:

```rust
#[export]
impl TokioAgentHandle {
    pub async fn prompt_text(&self, prompt: PromptText)
        -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn prompt_text_with_sink(
        &self,
        prompt: PromptText,
        sink: Arc<dyn AgentEventSink>,
    ) -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn prompt_text_sink_only(
        &self,
        prompt: PromptText,
        sink: Arc<dyn AgentEventSink>,
    ) -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn prompt_records(&self, records: Vec<AgentRecord>)
        -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn continue_run(&self) -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn retry_last_turn(&self) -> Result<TokioAgentRun, TokioAgentError>;
    pub async fn steer(&self, message: AgentRecord)
        -> Result<QueueReceipt, ControlError>;
    pub async fn follow_up(&self, message: AgentRecord)
        -> Result<QueueReceipt, ControlError>;
    pub async fn cancel(&self, run_id: RunId) -> Result<(), ControlError>;
    pub fn cancel_now(&self, run_id: RunId) -> Result<(), ControlError>;
    pub async fn subscribe(&self, sink: Arc<dyn AgentEventSink>)
        -> Result<EventSinkId, TokioAgentError>;
    pub async fn unsubscribe(&self, id: EventSinkId)
        -> Result<bool, TokioAgentError>;
    pub async fn reset_transcript(&self) -> Result<(), TokioAgentError>;
    pub async fn reset_all(&self) -> Result<(), TokioAgentError>;
    pub async fn snapshot(&self) -> Result<AgentSnapshot, TokioAgentError>;
    pub fn latest_snapshot(&self) -> AgentSnapshot;
    pub async fn wait_for_idle(&self) -> Result<(), TokioAgentError>;
    pub async fn shutdown(&self) -> Result<(), TokioAgentError>;
}
```

The current methods occupy
`crates/agentprism-runtime-tokio/src/lib.rs:202` through
`crates/agentprism-runtime-tokio/src/lib.rs:391`. `new`, `spawn`, and
`with_capacities` remain Rust-only because supervised construction belongs to
`TokioAgentFactory`. `snapshots()` remains Rust-only because it returns a raw
Tokio watch receiver (`crates/agentprism-runtime-tokio/src/lib.rs:368`). Each
omission is represented by F12's `#[skip]`-plus-reason entry rather than being
silently absent.

### 4.7 Concrete inputs and owned returns

Change exported `prompt_records` to take `Vec<AgentRecord>`; keep any generic
`IntoIterator` convenience as a distinctly named method in an unannotated
impl. The current generic method immediately collects a vector
(`crates/agentprism-runtime-tokio/src/lib.rs:230`,
`crates/agentprism-runtime-tokio/src/lib.rs:234`). `Vec<T>` is a documented
boundary collection.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#quick-reference]

Preserve current owned `AgentSnapshot`, `AgentRecord`, and `QueueReceipt`
returns/inputs (`crates/agentprism-runtime-tokio/src/lib.rs:254`,
`crates/agentprism-runtime-tokio/src/lib.rs:348`). No borrowed snapshots or
provider/executor trait-object seams enter Swift; the only trait objects in the
generated contract are the explicitly exported host callback protocols.

### 4.8 Public error partition

All boundary errors are cloneable, secret-free canonical Rust values. Their
stable variant partition is:

```rust
#[non_exhaustive]
pub enum TokioRuntimeError {
    Initialization { message: String },
    ShuttingDown,
    SupervisorClosed,
}

#[non_exhaustive]
pub enum TokioAssistantError {
    Closed,
    ConcurrentEventPoll,
    MissingTerminalEvent,
    Runtime(TokioRuntimeError),
    Invariant { message: String },
}

#[non_exhaustive]
pub enum NativeModelsError {
    InvalidConfig { field: String, message: String },
    CredentialStore { message: String },
    Provider { provider: ProviderId, message: String },
    Transport { message: String },
    Runtime(TokioRuntimeError),
}
```

`TokioAgentError` keeps current `Closed`, `Agent(AgentError)`,
`MissingRunFinished`, and `SnapshotInvariant`, replaces `NoRuntime` with nested
runtime initialization/shutdown reporting, and adds `ConcurrentEventPoll` plus
`ActorPoisoned { message }`. Provider failure/cancellation after establishment
never becomes one of these errors: it stays in terminal `AssistantEvent` or
`RunOutcome`. `RequestStartError` remains the pre-establishment direct-model
error (`crates/agentprism-ai/src/runtime.rs:42`). F12 and P0-05/P0-07 require
every variant and nested payload to appear in generated catches.

### 4.9 `Models` exported control-plane contract

The focused generated impl contains only owned catalog values and the canonical
Send auth operations:

```rust
#[export]
impl Models {
    pub fn models_owned(&self) -> Vec<ModelDescriptor>;
    pub fn model(&self, model_ref: ModelRef) -> Option<ModelDescriptor>;

    pub async fn check_auth(
        &self,
        provider_id: ProviderId,
        cancellation: CancellationToken,
    ) -> Result<Option<AuthCheck>, AuthError>;

    pub async fn login(
        &self,
        provider_id: ProviderId,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> Result<Credential, AuthError>;

    pub async fn logout(
        &self,
        provider_id: ProviderId,
        cancellation: CancellationToken,
    ) -> Result<(), AuthError>;
}
```

The existing `model` already returns an owned descriptor
(`crates/agentprism-ai/src/models.rs:248`); its argument becomes owned for the
generated impl. `models_owned` is the explicit concrete-collection companion
to the existing `Arc<[ModelDescriptor]>` snapshot. Provider registration,
refresh, middleware, typed API execution, auth overrides, stores, and builder
methods remain in unannotated impls with F12 reasons; direct ordinary execution
uses `TokioModelClient`, not an exported boxed stream.

## 5. Generated Swift consumer shapes

These examples are contract sketches; exact generated spelling is frozen by
the Phase-0 and surface-manifest goldens, not inferred from model memory.
Documented Rust async methods produce Swift async methods.
[https://www.boltffi.dev/docs/async.md | docs/boltffi-swift-bindings/docs-snapshot/async.md#methods]

### Pull plus outcome

```swift
let run = try await handle.promptText(prompt: prompt)
while let envelope = try await run.nextEvent() {
    consume(envelope)
}
let outcome = try await run.outcome()
```

### Lifecycle-preserving cancellation

```swift
run.cancel()
while let envelope = try await run.nextEvent() {
    consume(envelope)
}
let outcome = try await run.outcome()
```

### Abandon observation and settle

```swift
let outcome = try await run.cancelAndOutcome()
```

### Pull plus acknowledged sink

```swift
let run = try await handle.promptTextWithSink(prompt: prompt, sink: sink)
while let envelope = try await run.nextEvent() {
    consume(envelope)
}
let outcome = try await run.outcome()
```

### Sink-only

```swift
let run = try await handle.promptTextSinkOnly(prompt: prompt, sink: sink)
let outcome = try await run.outcome()
```

### Native control plane and direct model stream

```swift
let runtime = try TokioRuntimeOwner()
let models = try NativeModelsFactory(runtime: runtime, config: nativeConfig).build()
let auth = try await models.checkAuth(providerId: ProviderId("openai-codex"))
// If needed: try await models.login(providerId: ..., interaction: ...)

let modelClient = TokioModelClient(runtime: runtime, models: models)
let stream = try await modelClient.streamModel(request: request)
while let event = try await stream.nextEvent() {
    consume(event)
}
```

There is no required production Swift wrapper and no generated `AsyncStream`
for either authoritative event family.

## 6. Generated surface and value graph

The production manifest must cover every item in the following roots:

- `Models`, `NativeModelsFactory`, auth check/login values, and the required
  auth interaction callback;
- `TokioRuntimeOwner`, `TokioModelClient`, `TokioAgentFactory`,
  `TokioAgentHandle`, `TokioAgentRun`, `TokioAssistantStream`, and
  `CancellationToken`;
- prompt, state, snapshot, queue, receipt, event envelope, agent event,
  assistant event, outcome, model request/context/options, public error,
  usage/cost, tool call/update/output, replay, and all transitive IDs;
- `AgentEventSink`, conditionally only after P0-06 passes.

No generated surface may contain raw Tokio receivers, runtime handles,
`ModelRuntime`, `HttpTransport`, provider implementation traits,
`CredentialStore`, scheduler streams, local-executor types, or authoritative
BoltFFI subscriptions.

Appendix A is the path-specific completeness matrix. It is not satisfied by
serializing an unsupported value to a string or by introducing a duplicate
binding record. BoltFFI documents custom representations, but conversion
failure is not a recoverable substitute for canonical value fidelity.
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#representation-types]
[https://www.boltffi.dev/docs/custom-types.md | docs/boltffi-swift-bindings/docs-snapshot/custom-types.md#conversion-errors]

## 7. Acceptance tests

Rust contract tests live in current canonical crates. Private race hooks live
in `#[cfg(test)]` modules beside their machinery. The repository-owned XCTest
suite lives in `bindings/boltffi-swift-tests/` and compiles only against the
fresh generated package; production Swift source remains generated-only.
Test-only exported fixtures have a separate generation manifest and cannot be
present in the production module.

Tests 1–12 are the adopted review's original acceptance suite. Tests 13–19
retain the prior direct-stream, construction, value-graph, and pending-handoff
gates. Tests 20–27 are the mandatory F2–F8 additions. Tests 28–31 gate the
remaining audit blockers and the ready-value losslessness probe.

| # | Blocking semantic proof |
|---:|---|
| 1 | `DEFAULT_EVENT_CAPACITY * 3` uniquely indexed Agent events, deliberately slow Swift pulls, exact lossless order. |
| 2 | Exactly one `RunFinished` envelope is delivered last before active-observation EOF. |
| 3 | Malformed core EOF without `RunFinished` throws `MissingRunFinished`, never returns nil. The malformed source is test-only private machinery, not `ScriptedRuntime`, because normal Agent core synthesizes a terminal lifecycle. |
| 4 | Cancelling a pending Agent `nextEvent()` consumes no event; the next pull returns the unique event. |
| 5 | Calling `outcome()` without draining more than 128 events closes/discards observation, wakes a blocked sender, and cannot deadlock. |
| 6 | `prompt_text_sink_only` delivers more than 128 events to its sink with `RunChannels.events == None`, no pull, and no hidden drainer. |
| 7 | A held `RunFinished` sink permits the terminal envelope to reach pull observation but blocks EOF, `outcome`, and `waitForIdle` until acknowledgement. |
| 8 | A Swift sink cancels using its supplied `CancellationToken`; it does not call the mailbox from inside its own barrier. |
| 9 | Concurrent Agent pulls are explicitly rejected with `ConcurrentEventPoll`; the surviving cursor remains complete. This supersedes the old serialization expectation. |
| 10 | Generated production Swift/C/Rust glue contains no authoritative `EventSubscription` or `AsyncStream` for Agent/Assistant events and does contain both `nextEvent` methods. |
| 11 | The Rust-owned runtime remains alive through actor shutdown, actor-done, detached task leases, and last-lease release. |
| 12 | `AgentEventEnvelope.sequence` is exactly consecutive across two runs and a serialize/deserialize/persistence replay round-trip, with stable per-run IDs. |
| 13 | Direct assistant delivery exceeds three capacities under a slow Swift consumer with no loss/reordering. |
| 14 | Direct `Finished`, `Failed`, and `Cancelled` are delivered before active EOF; raw nonterminal EOF throws `MissingTerminalEvent`. |
| 15 | Cancelling a pending assistant pull consumes no event. |
| 16 | Direct model establishment cancellation, active lifecycle cancellation, `cancel_and_wait`, and abandoned observation all settle and release leases. |
| 17 | Captured no-network native construction/login/request for `openai-codex` / `gpt-5.6-sol` / `ReasoningLevel::Xhigh`, including fixture-backed persistent credential and device-code login paths. |
| 18 | Appendix A's complete canonical owned value graph generates and round-trips with exact variants, payloads, bytes, ordering, and all 13 replay roots. `ControlError::QueueFull.capacity` is exact. |
| 19 | Cancelling Agent establishment after actor acceptance but before `RunEstablishmentGuard::handoff` cancels/settles the run, restores idle, and releases leases for prompt, records, continue, retry, and pull-plus-sink. |
| 20 | F2 deterministic check-to-send race: receiver drops after idle false but before acceptance send; idle is restored and the next run is accepted. |
| 21 | F3 Agent post-ready/pre-Swift-retain release invokes established-run `Drop`, cancels, settles idle, and releases the actor run lease. |
| 22 | F3 Assistant post-ready/pre-Swift-retain release invokes stream `Drop`, cancels the producer, and releases its lease. |
| 23 | F4 provider-pending-forever teardown: drop established assistant class without `cancelAndWait`; runtime reaches zero leases and exits. |
| 24 | F5 naming: `promptTextWithSink` returns and losslessly delivers pull events plus sink calls; `promptTextSinkOnly` creates no sender and still settles. |
| 25 | F6 EOF matrix: active observation requires consumer terminal delivery; outcome-closed and never-installed observation require only internally validated terminal/completion/sink settlement. |
| 26 | F7 two simultaneous pulls on each run/assistant class yield one `ConcurrentEventPoll`, no duplicate/omitted event, and a reusable cursor after cancelled waits. |
| 27 | F8 forced post-`RunStarted` `SnapshotInvariant` dispatches the rejected envelope, reports the error, then a second run starts at the exact next sequence; forced pre-`RunStarted` failure poisons the actor. |
| 28 | F12 production and fixture surface manifests are complete, every `#[skip]` has a reason, and source/generated additions or omissions fail CI. |
| 29 | F11 all shared Rust-backed classes and Send callback protocols compile and execute under Swift 6 strict concurrency with warnings as errors and generator-owned `Sendable` treatment. |
| 30 | F9/F10 a fresh checkout runs `cargo xtask package-apple`, runs all repository-owned XCTest against the generated package, validates three platform slices/five target architectures, names, and symbols, and reproduces the normalized artifact manifest. |
| 31 | P0-10 Agent and Assistant pulls paused after `Ready(Some(event))` but before Swift retention either deliver that exact event or leave it as the next event; cancellation never creates a sequence/content gap. |

Every race test uses a barrier at the named transition. A timeout alone is not
proof. Every drop test observes cancellation settlement and lease release, not
merely receiver closure.

## 8. Phase-0 probe and implementation order

### Phase 0 — disposable BoltFFI capability probe

Pin `=0.30.1` and resolve P0-01 through P0-12 in a non-shipping probe target.
The target is `tools/boltffi-capability-probe`, and its normalized generated
goldens live under `docs/boltffi-swift-bindings/phase-0-goldens/`. It must prove
root/dependency scanning, `cfg_attr`, tuple shapes, nested errors,
owned class callback arguments, all non-exhaustive enums, the full value graph,
Swift 6 concurrency, generated-surface extraction, both completed-class
handoff drop tests, and the completed-event handoff race. Record exact commands,
versions, generated goldens, and probe results under
`docs/boltffi-swift-bindings/phase-0-results.md`.

If any item fails, stop before broad annotations. Allowed responses are a
pinned generator/source fix, a legitimate canonical Rust API improvement, or
an owner decision. A binding-only envelope, duplicate enum, JSON command, or
required Swift wrapper is not an allowed workaround.

### Slice 1 — Bolt capability/root-package/Swift-6 production contract

Productionize the successful Phase-0 mechanics: create `agentprism-native`,
`boltffi.toml`, exact dependencies/build script, the dependency graph from F9,
production/test surface manifests, the repository-local pinned tool bootstrap,
and the F11 generator-owned concurrency change if P0-09 required it. Apply only
enough proven annotations to demonstrate production multi-crate discovery and
compile one default-thread-safe class plus one Send callback under Swift 6
strict concurrency. Gate: F12 checker passes, Swift 6 smoke compilation passes,
and non-Bolt builds of canonical crates remain clean.

### Slice 2 — `TokioAgentRun` pull/outcome/cancellation

Implement interior synchronization, reusable completion, F2
send-then-restore, establishment guard, established `Drop`, EOF state model,
concurrent-poll rejection, outcome abandonment, cancel APIs, and concrete
record inputs. Gate: tests 1–5, 9, 19–21, 25–26 Rust halves.

### Slice 3 — envelope promotion and sink semantics

Promote `AgentEventEnvelope` once before fan-out, implement the F8 invariant
path, rewrite `AgentEventSink`, preserve `prompt_text_with_sink`, add
`prompt_text_sink_only`, and keep sink barriers. Gate: tests 6–8, 12, 24, 27.

### Slice 4 — Rust-owned runtime

Implement `TokioRuntimeOwner`, supervisor, task leases, actor-done, graceful
shutdown, `TokioModelClient`, and `TokioAgentFactory` separation. Gate: test 11
and all Agent drop/lease assertions.

### Slice 5 — direct model stream

Implement `TokioAssistantStream`, bounded pull, establishment guard,
established `Drop`, pending-provider abandonment, terminal validation,
concurrent-poll rejection, and `TokioModelClient::stream_model`. Gate: tests
13–16, 22–23, 26.

### Slice 6 — production provider/auth construction

Add `agentprism-transport-reqwest`, provider-neutral `NativeModelsFactory`,
persistent credentials, concrete `Models` auth/catalog methods, and required
auth callbacks. Provider leaves retain `Arc<dyn HttpTransport>`. Gate: test 17
with no live network and API-key-only construction rejected by surface review.

### Slice 7 — complete value graph

Annotate only Phase-0-proven canonical types. Execute Appendix A and all
non-exhaustive/error/`usize` cases. Gate: tests 10, 18, 28, and production
surface completeness.

### Slice 8 — Apple packaging

Implement `cargo xtask package-apple`, the generator-owned Swift 6 fix if
required, three platform slices containing five target architectures,
XCFramework/SwiftPM emission, XCTest, artifact manifest, and fresh-checkout
reproducibility. Gate: tests 29–31. Do not publish crates.

## 9. Commitment checklist

Implementation is complete only when:

- all P0 items have recorded empirical answers for the exact pin;
- all 31 acceptance tests pass in Rust and generated Swift where applicable;
- the production surface manifest is complete and contains no authoritative
  BoltFFI subscription;
- the native construction path preserves `Models` auth/catalog/provider
  control and the lower-level Agent still receives only its narrow runtime;
- `TokioAgentRun` and `TokioAssistantStream` drop safely in pending,
  post-ready/pre-retain, active, terminal, and abandoned states;
- the generated Swift module compiles under Swift 6 strict concurrency without
  a required handwritten wrapper;
- `cargo xtask package-apple` reproduces the validated package from a clean
  checkout; and
- `bindings/agentprism-ffi` remains unchanged until a separate migration
  decision.

## Appendix A — complete owned-value graph gate

P0-04, P0-05, P0-07, P0-08, P0-11, and acceptance test 18 use independent,
sentinelized canonical roots. No root may stand in for another simply because
they share a leaf type.

### A.1 Tuple/newtype and enum syntax

Generate and round-trip:

- macro IDs plus `QueueSequence`, `EventSinkId`, `Timestamp`, `Currency`,
  `ReplayDropReason`, `OrderedJsonString`, `OrderedJsonObject`, and
  `OrderedJsonArray` (`crates/agentprism-ai/src/ids.rs:6`,
  `crates/agentprism-core/src/control.rs:19`,
  `crates/agentprism-runtime-tokio/src/lib.rs:35`,
  `crates/agentprism-ai/src/ids.rs:133`,
  `crates/agentprism-ai/src/usage.rs:88`,
  `crates/agentprism-ai/src/handoff.rs:44`,
  `crates/agentprism-ai/src/json_compat.rs:24`);
- every variant of `Message`, `AgentRecord`, `DiagnosticErrorCode`,
  `ConstrainedSampling`, `OrderedJsonValue`, `ReplayTarget`, `OpaquePayload`,
  and `ReplayDataOperation` (`crates/agentprism-ai/src/messages.rs:32`,
  `crates/agentprism-core/src/state.rs:62`,
  `crates/agentprism-ai/src/messages.rs:178`,
  `crates/agentprism-ai/src/messages.rs:334`,
  `crates/agentprism-ai/src/json_compat.rs:324`,
  `crates/agentprism-ai/src/replay.rs:179`,
  `crates/agentprism-ai/src/replay.rs:276`,
  `crates/agentprism-ai/src/streaming.rs:563`);
- tuple/nested errors, every in-scope non-exhaustive enum identified by P0-07,
  and `ControlError::QueueFull { capacity: usize }`
  (`crates/agentprism-core/src/control.rs:68`,
  `crates/agentprism-runtime-tokio/src/lib.rs:70`).

Ordered JSON uses nested arrays, insertion-ordered objects, exact numeric text,
and exact UTF-16 units. `usize` cases include zero, current event capacity, and
`usize::MAX`. `DEFAULT_COMMAND_CAPACITY` and `DEFAULT_EVENT_CAPACITY` remain
Rust-only unless an explicit later surface decision is made
(`crates/agentprism-runtime-tokio/src/lib.rs:30`).

### A.2 Path-specific request/event/snapshot roots

Use distinct sentinels for:

- a direct `ModelRequest` containing assistant and tool-result messages,
  `ToolCall.arguments`, `DeferredHandle.data`, diagnostic string/number/details,
  usage/cost/timestamp, `ToolResultMessage.details`, `ToolSpec.parameters`,
  constrained sampling, grammar variants, headers, and an API patch
  (`crates/agentprism-ai/src/runtime.rs:13`,
  `crates/agentprism-ai/src/messages.rs:300`,
  `crates/agentprism-ai/src/messages.rs:311`,
  `crates/agentprism-ai/src/deferred.rs:19`);
- standalone `AssistantEvent::DiagnosticAdded`, `Finished`, `Failed`, and
  `Cancelled`, plus every one nested in `AgentEvent::AssistantUpdate`
  (`crates/agentprism-ai/src/streaming.rs:360`,
  `crates/agentprism-core/src/events.rs:113`);
- assistant and tool-result `AgentEvent::MessageCommitted`, standalone
  `AgentRecord`, direct `AgentState`, and `AgentSnapshot.state`;
- `ToolExecutionStarted`, `ToolExecutionUpdated`, and
  `ToolExecutionFinished`;
- `ContextPrepared` with `OpaqueReplayDropped`;
- `AgentSnapshot.streaming = Some(AssistantMessageSnapshot)` whose partial
  tool-call arguments, deferred data, diagnostics, replay, usage/cost,
  timestamp, and terminal message differ from its committed transcript
  (`crates/agentprism-core/src/state.rs:180`,
  `crates/agentprism-ai/src/streaming.rs:1674`).

Assert exact `serde_json::Value` structure, raw JSON text, map insertion/order,
byte arrays, option presence, enum identity, IDs, terminal cases, and every
owned field. Retain separate probes for `serde_json::Number`, `BTreeSet`,
`Arc<[T]>`, `IndexMap`, and `i128`.

### A.3 Auth/control-plane callback roots

Generate the complete Send-family auth surface used by the exported `Models`
methods, with distinct sentinels for:

- `AuthCheck`, `AuthSource`, `Credential`, `CredentialType`,
  `ApiKeyCredential`, `OAuthCredential`, `ProviderOAuthExtra`, `SecretString`,
  `AuthHostCapabilities`, every `AuthPrompt`/`AuthAnswer` variant, every
  `AuthEvent` variant, and every `AuthInteractionError` variant
  (`crates/agentprism-ai/src/provider.rs:209`,
  `crates/agentprism-ai/src/auth.rs:920`,
  `crates/agentprism-ai/src/auth.rs:948`,
  `crates/agentprism-ai/src/auth.rs:983`,
  `crates/agentprism-ai/src/auth.rs:1002`,
  `crates/agentprism-ai/src/auth.rs:1041`);
- `RedirectReceiverRequest`, every `RedirectStrategy`, `AuthHtmlPage`, and
  `RedirectArrival`, including IPv4/IPv6, URL, timestamp, and success/failure
  HTML sentinels (`crates/agentprism-ai/src/auth.rs:1130`,
  `crates/agentprism-ai/src/auth.rs:1137`,
  `crates/agentprism-ai/src/auth.rs:1175`,
  `crates/agentprism-ai/src/auth.rs:1197`);
- the full `AuthInteraction` callback and its returned `RedirectReceiver`,
  including capabilities, every prompt answer, notifications, receiver
  creation, owned receive, cancellation, and all error returns
  (`crates/agentprism-ai/src/auth.rs:1084`,
  `crates/agentprism-ai/src/auth.rs:1206`).

The docs explicitly map `Duration` and `url::Url`; their precise generated
round-trips are still tested here. `IpAddr`, callback-object returns,
boxed-self receivers, and the current borrowed `redirect_uri` return remain
P0-06/P0-08 blockers rather than assumed support.
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#duration]
[https://www.boltffi.dev/docs/types.md | docs/boltffi-swift-bindings/docs-snapshot/types.md#url]

`Models::login` retains its canonical `Credential` return rather than gaining a
second binding record. The generated security test proves that debug/error
paths preserve `SecretString` redaction; the exact owned-value test proves the
credential variants round-trip without using JSON or silently omitting
provider-specific OAuth data.

### A.4 Thirteen independent replay-envelope roots

Each root contains a distinct nonempty `ReplayEnvelope`, distinct complete
`ReplayScope`, ordered items, every item field, all four `ReplayTarget` forms,
and all three `OpaquePayload::{Utf8,Bytes,JsonBytes}` forms
(`crates/agentprism-ai/src/replay.rs:13`,
`crates/agentprism-ai/src/replay.rs:88`,
`crates/agentprism-ai/src/replay.rs:135`,
`crates/agentprism-ai/src/replay.rs:179`,
`crates/agentprism-ai/src/replay.rs:276`).

1. Direct `ModelRequest.context.messages` assistant replay.
2. Direct `AssistantEvent::Finished.message.replay`.
3. Direct `AssistantEvent::Failed.message.replay`.
4. Direct `AssistantEvent::Cancelled.message.replay`.
5. `AgentEvent::AssistantUpdate(Finished)` terminal-message replay.
6. `AgentEvent::AssistantUpdate(Failed)` terminal-message replay.
7. `AgentEvent::AssistantUpdate(Cancelled)` terminal-message replay.
8. `AgentEvent::MessageCommitted` assistant replay.
9. Standalone assistant `AgentRecord` replay.
10. Direct `AgentState.transcript` assistant replay.
11. `AgentSnapshot.state.transcript` assistant replay.
12. `AgentSnapshot.streaming.replay`.
13. `AgentSnapshot.streaming.terminal_message.replay`.

The relevant canonical paths are
`crates/agentprism-ai/src/messages.rs:128`,
`crates/agentprism-ai/src/streaming.rs:521`,
`crates/agentprism-ai/src/streaming.rs:528`,
`crates/agentprism-ai/src/streaming.rs:534`,
`crates/agentprism-core/src/events.rs:113`,
`crates/agentprism-core/src/events.rs:120`,
`crates/agentprism-core/src/state.rs:33`,
`crates/agentprism-core/src/state.rs:184`,
`crates/agentprism-core/src/state.rs:188`,
`crates/agentprism-ai/src/streaming.rs:1697`, and
`crates/agentprism-ai/src/streaming.rs:1705`.

Separately generate standalone and nested `ReplayItemStarted` for all four
targets and `ReplayData` for all five operations, with different sentinels for
direct and nested values (`crates/agentprism-ai/src/streaming.rs:474`,
`crates/agentprism-ai/src/streaming.rs:488`,
`crates/agentprism-ai/src/streaming.rs:563`). An empty envelope, shared sentinel
set, JSON-string replacement, or inference from another root fails the gate.
