# Rust API inventory for the lower-level Agent Swift SDK

## Scope, authority, and classification

This is a current-tree Rust API inventory for the lower-level Agent SDK
milestone adopted on 2026-08-26. The milestone covers the concrete Tokio actor
boundary and the direct model-stream path. The production coding-agent SDK over
the durable harness is a separate future milestone.

The source baseline inspected here is branch `boltffi-design` at
`f7f65044b42d7a23b0765f3c7ac750e2415b85d1`. The only dirty path at inventory
time was this document; crate source was not modified.

This document makes no claim about what BoltFFI supports, requires, generates,
or forbids. It records only Rust code and tests observed in the current
`agentprism-*` tree.

The public modules of `agentprism-core` are glob-re-exported at
`crates/agentprism-core/src/lib.rs:20-29`. The public modules of
`agentprism-ai` are glob-re-exported at
`crates/agentprism-ai/src/lib.rs:46-77`. Public visibility is therefore much
broader than this milestone's consumer boundary.

Classification in this inventory means:

- **core**: required by the ordinary lower-level Agent flow or direct model
  stream in this milestone.
- **extended**: a legitimate consumer capability adjacent to that basic flow,
  such as restoration, catalog/auth control, deferred execution, or
  acknowledged callbacks.
- **excluded**: provider/tool/policy authoring, borrowed or generic low-level
  seams, Local executor twins, test fixtures, raw executor objects, or the
  separately scoped production coding-agent layer.

Classification follows the owned value graph, not how frequently a branch is
used. If a type is stored directly or transitively in a **core** record, that
type is also **core**, including optional option fields, optional constrained
sampling, deferred-submission preferences, and extension payloads. A trait or
builder that authors one of those values can still be **excluded**.

Execution-family labels are **shared**, **Send**, **Local**, and
**Tokio/Send**.

## Current ordinary-consumer routes

### Lower-level Agent route

The ordinary Rust assembly path is:

```text
Models (or another Arc<dyn ModelRuntime>)
    -> Agent::new(runtime, AgentState, ToolRegistry)
    -> TokioAgentHandle::new(agent)
    -> TokioAgentRun
    -> next_event() and outcome()
```

`Agent::new` accepts `Arc<dyn ModelRuntime>`, `AgentState`, and `ToolRegistry`
at `crates/agentprism-core/src/run.rs:217-221`. The Tokio test constructs that
exact graph at `crates/agentprism-runtime-tokio/tests/m2_2_handle.rs:168-175`
and exercises prompt, steering, follow-up, snapshot, cancellation, outcome,
reset, continue, retry, and shutdown at
`crates/agentprism-runtime-tokio/tests/m2_2_handle.rs:447-497`.

There is no `examples/` directory in `crates/agentprism-core` in the current
tree. Its ordinary use is demonstrated by the integration tests: prompt/event
ordering at `crates/agentprism-core/tests/m2_2_run.rs:124-158`, the borrowed
stream's observational behavior at
`crates/agentprism-core/tests/m2_2_run.rs:232-244`, restoration at
`crates/agentprism-core/tests/m2_1_state.rs:416-441`, typed tools at
`crates/agentprism-core/tests/m2_3_tools.rs:601-660`, and policy injection at
`crates/agentprism-core/tests/m2_4_policies.rs:132-183`.

The current Rust construction path is not yet a self-contained foreign
construction boundary: `TokioAgentHandle::new` requires a fully assembled
`Agent`, while `Agent::new` requires a Rust trait object and tool registry. The
inventory records these facts without choosing the Design-phase factory shape.

### Direct model-stream route

The current direct path is:

```text
Models::stream_simple(ModelRequest, CancellationToken)
    -> SendBoxFuture<Result<AssistantStream, RequestStartError>>
    -> AssistantStream: Stream<Item = AssistantEvent>
```

`Models::stream_simple` has that owned request/result shape, while its explicit
auth sibling adds `AuthResolutionOverrides`, at
`crates/agentprism-ai/src/models.rs:760-780`.
`ModelRuntime::stream` is the narrow trait form at
`crates/agentprism-ai/src/runtime.rs:86-94`, and the `Models` implementation
delegates to `stream_simple` at
`crates/agentprism-ai/src/models.rs:1399-1406`. `AssistantStream` owns a
`SendBoxStream<'static, AssistantEvent>`, implements `Stream`, and fuses after
a terminal event or EOF at
`crates/agentprism-ai/src/streaming.rs:1898-1961`. The runtime contract test
constructs and polls this exact path at
`crates/agentprism-ai/tests/m1_3_runtime.rs:147-160`.

The current tree has no concrete Tokio-owned assistant-run class or async pull
method corresponding to `TokioAgentRun`; direct callers must poll the Rust
`AssistantStream` itself. That is the current construction gap for the Design
phase, not a reason to replace `AssistantEvent` with another event hierarchy.

### Existing C example: evidence, not target shape

`bindings/agentprism-ffi/examples/scripted_host.rs` uses the legacy C ABI. It
creates model and agent handles at
`bindings/agentprism-ffi/examples/scripted_host.rs:89-92`, starts a run with a
callback at `bindings/agentprism-ffi/examples/scripted_host.rs:100-108`,
cancels re-entrantly after `run_started` at
`bindings/agentprism-ffi/examples/scripted_host.rs:21-38`, waits for
`run_finished` and checks monotonic sequence values at
`bindings/agentprism-ffi/examples/scripted_host.rs:110-130`, drives an auth
session at `bindings/agentprism-ffi/examples/scripted_host.rs:133-187`, and
destroys all handles at `bindings/agentprism-ffi/examples/scripted_host.rs:189-194`.

Its JSON configuration, callback JSON, numeric run identity, and hand-built
envelope checks are evidence of required operations only. They are not the
ordinary Rust API selected by R1/R2.

## Core: `agentprism-runtime-tokio` actor boundary

### Actor, run, and control surface

| Item | Current source | Family | Class | Exact current role |
|---|---|---:|---:|---|
| `TokioAgentHandle` | `crates/agentprism-runtime-tokio/src/lib.rs:148-164` | Tokio/Send | core | Cloneable handle holding the command sender, snapshot and idle watches, direct `AgentControl`, and per-run event capacity. |
| `TokioAgentHandle::{new,spawn}` | `crates/agentprism-runtime-tokio/src/lib.rs:166-175` | Tokio/Send | core | Start an actor from an already assembled `Agent`. |
| `TokioAgentHandle::with_capacities` | `crates/agentprism-runtime-tokio/src/lib.rs:177-200` | Tokio/Send | extended | Clamps capacities to at least one, acquires the current Tokio handle, creates channels, and spawns `actor_loop`. |
| `prompt_text` | `crates/agentprism-runtime-tokio/src/lib.rs:202-209` | Tokio/Send | core | Starts a text/image run and returns `TokioAgentRun`. |
| `prompt_text_with_sink` | `crates/agentprism-runtime-tokio/src/lib.rs:211-227` | Tokio/Send | extended | Starts one run with a run-scoped acknowledged sink and still returns the normal observational run. |
| `prompt_records` | `crates/agentprism-runtime-tokio/src/lib.rs:229-240` | Tokio/Send | core | Collects an iterator into `Vec<AgentRecord>` before submitting the command. |
| `continue_run`, `retry_last_turn` | `crates/agentprism-runtime-tokio/src/lib.rs:242-252` | Tokio/Send | core | Start follow-on actor-owned runs. |
| `steer`, `follow_up`, `cancel` | `crates/agentprism-runtime-tokio/src/lib.rs:254-294` | Tokio/Send | core | Send serialized mailbox commands and await acknowledgements. |
| `cancel_now` | `crates/agentprism-runtime-tokio/src/lib.rs:296-303` | Tokio/Send | core | Calls the cloneable core control directly for re-entrant cancellation while a sink is awaited. |
| `subscribe`, `unsubscribe`, `EventSinkId` | `crates/agentprism-runtime-tokio/src/lib.rs:35-37`, `crates/agentprism-runtime-tokio/src/lib.rs:305-326` | Tokio/Send | extended | Manage actor-wide acknowledged sinks in registration order. |
| `reset_transcript`, `reset_all` | `crates/agentprism-runtime-tokio/src/lib.rs:328-346` | Tokio/Send | core | Actor-serialized reset operations. |
| `snapshot`, `latest_snapshot` | `crates/agentprism-runtime-tokio/src/lib.rs:348-366` | Tokio/Send | core | Barrier snapshot and re-entrant last-published owned snapshot. |
| `snapshots` | `crates/agentprism-runtime-tokio/src/lib.rs:368-371` | Tokio/Send | excluded | Returns a raw `tokio::sync::watch::Receiver<AgentSnapshot>`. |
| `wait_for_idle`, `shutdown` | `crates/agentprism-runtime-tokio/src/lib.rs:373-392` | Tokio/Send | core | Idle/sink-settlement barrier and graceful actor stop. |
| `TokioAgentRun` | `crates/agentprism-runtime-tokio/src/lib.rs:120-146` | Tokio/Send | core | Currently owns `mpsc::Receiver<AgentEvent>` plus a consuming oneshot completion receiver. |
| `TokioAgentRun::events` | `crates/agentprism-runtime-tokio/src/lib.rs:131-135` | Tokio/Send | excluded | Returns a borrowed raw Tokio receiver. |
| `TokioAgentRun::next_event` | `crates/agentprism-runtime-tokio/src/lib.rs:137-140` | Tokio/Send | core | Currently requires `&mut self` and returns `Option<AgentEvent>` without surfacing actor completion errors at EOF. |
| `TokioAgentRun::outcome` | `crates/agentprism-runtime-tokio/src/lib.rs:142-145` | Tokio/Send | core | Currently consumes the run and awaits its one-shot completion. |
| `TokioAgentError` | `crates/agentprism-runtime-tokio/src/lib.rs:67-118` | Tokio/Send | core | `NoRuntime`, `Closed`, nested `Agent`, `MissingRunFinished`, or `SnapshotInvariant`. |
| capacities | `crates/agentprism-runtime-tokio/src/lib.rs:29-33` | Tokio/Send | extended | Command capacity 64 and event capacity 128. |

### `AgentEventSink` and delivery semantics

The current sink is a synchronous trait method returning a boxed asynchronous
acknowledgement future:

```rust
pub trait AgentEventSink: Send + Sync + 'static {
    fn on_event(
        &self,
        event: AgentEvent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, ()>;
}
```

This is the exact shape at
`crates/agentprism-runtime-tokio/src/lib.rs:39-52`; closures receive the same
implementation at `crates/agentprism-runtime-tokio/src/lib.rs:54-65`.

For each core event, `drive_run` first applies it to the actor's snapshot mirror
and publishes that snapshot, remembers a `RunFinished` outcome, and then calls
`dispatch_event` (`crates/agentprism-runtime-tokio/src/lib.rs:750-774`).
`dispatch_event` awaits the observational channel send, then each registered
sink in vector order, then the run-scoped sink
(`crates/agentprism-runtime-tokio/src/lib.rs:832-853`). The sink barrier test
holds a callback open and proves later events do not advance at
`crates/agentprism-runtime-tokio/tests/m2_2_handle.rs:236-390`; the idle test
holds the `RunFinished` callback and proves `wait_for_idle` does not resolve at
`crates/agentprism-runtime-tokio/tests/m2_2_handle.rs:393-439`.

The observational send is bounded but awaited. If its receiver is closed,
`dispatch_event` removes that sender and continues with sinks. If it remains
open and full, the actor remains in the awaited send before invoking either
sink family (`crates/agentprism-runtime-tokio/src/lib.rs:839-852`).

### Run acceptance and completion machinery

Every `request_run` allocates an event channel, completion oneshot, and
acceptance oneshot, sends a `RunChannels` command, waits for acceptance, and
then returns `TokioAgentRun` (`crates/agentprism-runtime-tokio/src/lib.rs:394-424`).
This is true even when `prompt_text_with_sink` supplies a sink.

`accept_run` checks whether the acceptance receiver is already closed, sets
idle to false, and returns whether sending acceptance succeeded
(`crates/agentprism-runtime-tokio/src/lib.rs:699-708`). A failed send after the
check currently has no compensating `idle_tx.send(true)`. Normal settlement
publishes the final agent snapshot, sets idle true, sends completion, resolves
queued shutdown responses, and reports whether the actor should exit
(`crates/agentprism-runtime-tokio/src/lib.rs:720-734`).

## Core owned Agent values

### State, input, lifecycle, and control values

| Item group | Current source | Family | Class | Role |
|---|---|---:|---:|---|
| `AgentState`, `AgentState::new` | `crates/agentprism-core/src/state.rs:18-50` | shared | core | System prompt, `ModelRef`, reasoning, and durable transcript. Constructor has `impl Into<String>`. |
| `AgentRecord` and inspection helpers | `crates/agentprism-core/src/state.rs:53-72`, `crates/agentprism-core/src/state.rs:156-172` | shared | core | Canonical message or custom type name plus exact `Box<RawValue>`. |
| `AgentSnapshot`, `AgentSnapshot::new` | `crates/agentprism-core/src/state.rs:174-203` | shared | core | Owned durable state, next event sequence, assistant snapshot, and pending tool-call IDs. |
| schema/sequence constants | `crates/agentprism-core/src/state.rs:9-16` | shared | extended | State version, snapshot version, and initial sequence. |
| `AgentInput`, `PromptImage`, `PromptText` | `crates/agentprism-core/src/run.rs:66-71`, `crates/agentprism-core/src/run.rs:159-175` | shared | core | Prompt record batch and text/image convenience values. |
| `MessageRole`, `TurnOutcome`, `RunOutcome` | `crates/agentprism-core/src/events.rs:15-75` | shared | core | Lifecycle role, per-turn result, and completed/failed/cancelled terminal outcome. |
| `AgentEvent` | `crates/agentprism-core/src/events.rs:77-155` | shared | core | Eleven ordered lifecycle variants, including lossless nested `AssistantEvent`. |
| `AgentEventEnvelope` | `crates/agentprism-core/src/events.rs:327-336` | shared | core | Existing canonical sequence/run/event persistence value; the Tokio actor does not currently send it. |
| `QueueSequence`, `QueueKind`, `QueueReceipt`, `ControlError` | `crates/agentprism-core/src/control.rs:16-22`, `crates/agentprism-core/src/control.rs:24-32`, `crates/agentprism-core/src/control.rs:56-83` | shared | core | Owned results/errors of steering and follow-up. |
| `QueueDrainMode` | `crates/agentprism-core/src/control.rs:34-43` | shared | extended | One/all queue draining configuration. |
| `AgentPhase` | `crates/agentprism-core/src/run.rs:26-64` | shared | extended | Detailed transient state-machine phase. |
| `AgentError` | `crates/agentprism-core/src/error.rs:6-97` | shared | core | Configuration, restoration, active-run, replay, and invariant errors. |

`CommittedEventReplay` validates consecutive envelope sequence and matching run
identity, commits only `MessageCommitted`, and advances its required sequence at
`crates/agentprism-core/src/replay.rs:10-108`. This confirms that the existing
`AgentEventEnvelope` is functional canonical data, not an unused declaration.

### Event/request-carried tool values versus tool authoring

The following owned tool values are **core because they are carried by scoped
events, transcript records, or `ModelRequest::context`**, independently of
whether Swift can author executable tools in this milestone:

| Value | Containing core path | Current source | Class |
|---|---|---|---:|
| `ToolCall` | `AgentEvent::ToolExecutionStarted`, `ContentBlock::ToolCall`, and assistant tool metadata | `crates/agentprism-core/src/events.rs:124-128`; `crates/agentprism-ai/src/messages.rs:277-307` | core |
| `ToolUpdate` | `AgentEvent::ToolExecutionUpdated::update` | `crates/agentprism-core/src/events.rs:129-135`; `crates/agentprism-core/src/tools.rs:83-130` | core |
| `ToolOutput` | `AgentEvent::ToolExecutionFinished::result` | `crates/agentprism-core/src/events.rs:136-144`; `crates/agentprism-core/src/tools.rs:44-81` | core |
| `ToolResultContent` | Nested in `ToolOutput`, `ToolUpdate`, and `ToolResultMessage` | `crates/agentprism-core/src/tools.rs:47-58`; `crates/agentprism-core/src/tools.rs:89-100`; `crates/agentprism-ai/src/messages.rs:220-242`; `crates/agentprism-ai/src/messages.rs:420-440` | core |
| `ToolSpec` | `Context::tools`, therefore `ModelRequest::context` | `crates/agentprism-ai/src/messages.rs:309-326`; `crates/agentprism-ai/src/messages.rs:464-488`; `crates/agentprism-ai/src/runtime.rs:11-20` | core |
| `ConstrainedSampling`, `ConstrainedSamplingConfig`, `JsonSchemaStrictMode`, `GrammarFormat`, `GrammarVariants` | Optional but direct/transitive `ToolSpec::constrained_sampling` graph | `crates/agentprism-ai/src/messages.rs:320-418` | core |

Swift-authored tools are outside this milestone. The distinct **excluded
tool-authoring/execution graph** is `ToolCallContext`, `ToolExecutionMode`,
`ToolError`, `ToolUpdateError`, `ToolUpdateSink`, `Tool`,
`ToolArgumentPreparer`, `TypedTool<I, F>`, and `ToolRegistry`. Their definitions
and callback/registry relationships are at
`crates/agentprism-core/src/tools.rs:18-42`,
`crates/agentprism-core/src/tools.rs:132-217`,
`crates/agentprism-core/src/tools.rs:238-253`,
`crates/agentprism-core/src/tools.rs:271-379`, and
`crates/agentprism-core/src/tools.rs:489-550`. `ToolRegistry` remains an
internal construction dependency of `Agent::new` at
`crates/agentprism-core/src/run.rs:215-246`; that does not make the executable
tool-authoring API part of the scoped foreign consumer boundary.

### Bare `Agent`: canonical Rust seam, excluded direct boundary

`Agent` is a Send state machine containing an `Arc<dyn ModelRuntime>`, state,
tools, control, policies, and run scratch
(`crates/agentprism-core/src/restore.rs:124-148`). `Agent::new` is at
`crates/agentprism-core/src/run.rs:215-246`; ordered restoration and owned
snapshot access are at `crates/agentprism-core/src/restore.rs:150-217`.

Its run methods are deliberately **excluded** from the concrete SDK boundary:

- `run`, `prompt_text`, and iterator-generic `prompt_records` return
  `SendBoxStream<'a, AgentEvent>` borrowing `&'a mut self`
  (`crates/agentprism-core/src/run.rs:374-401`).
- `continue_run` and `retry_last_turn` return fallible borrowed streams
  (`crates/agentprism-core/src/run.rs:403-446`).
- the durable-recovery methods `resume_interrupted_turn`,
  `resume_completed_turn`, and `resume_completed_turn_stream` belong to the
  separate durable-recovery/product scope
  (`crates/agentprism-core/src/run.rs:448-495`).
- mutable state/options and policy/scheduler setters expose borrowed and trait
  object composition (`crates/agentprism-core/src/run.rs:303-371`).

`AgentControl` is the low-level cloneable queue/cancellation capability at
`crates/agentprism-core/src/control.rs:105-109`, with steering, follow-up, and
cancellation at `crates/agentprism-core/src/control.rs:153-174`. The scoped
consumer operations are exposed through `TokioAgentHandle`; direct
`AgentControl` construction remains excluded.

## `agentprism-ai` seams visible at the boundary

### Runtime and direct stream

| Item | Current source | Family | Class | Role |
|---|---|---:|---:|---|
| `ModelRequest` | `crates/agentprism-ai/src/runtime.rs:11-20` | shared | core | Owned `ModelRef`, `Context`, and `SimpleGenerationOptions`. |
| `RequestStartErrorKind`, `RequestStartError` | `crates/agentprism-ai/src/runtime.rs:22-84` | shared | core | Pre-stream failure only; established-stream failures/cancellation are terminal events. |
| `ModelRuntime` | `crates/agentprism-ai/src/runtime.rs:86-94` | Send | excluded as a host-authored boundary; core internally | Narrow object-safe execution capability consumed by `Agent`. |
| `SendBoxFuture`, `SendBoxStream` | `crates/agentprism-ai/src/async_types.rs:10-17` | Send | excluded carriers | Generic Rust executor carriers used behind concrete APIs. |
| `AssistantStream` | `crates/agentprism-ai/src/streaming.rs:1898-1961` | Send | core semantic source | Owned, fused `AssistantEvent` stream returned by `Models`/`ModelRuntime`. |
| `AssistantEvent` and inspectors | `crates/agentprism-ai/src/streaming.rs:356-558` | shared | core | Twenty-one lossless normalized event forms and terminal-message inspection. |
| `ContentBlockKind`, `ReplayDataOperation`, `CancellationReason` | `crates/agentprism-ai/src/streaming.rs:343-354`, `crates/agentprism-ai/src/streaming.rs:560-600` | shared | core | Nested stream event values, including UTF-8, opaque bytes, and JSON bytes. |
| `AssistantMessageSnapshot` | `crates/agentprism-ai/src/streaming.rs:1671-1706` | shared | core | Scratch-free owned partial/terminal assistant view nested in `AgentSnapshot`. |
| `AssistantAssembler`, `AssemblyError` | `crates/agentprism-ai/src/streaming.rs:800-819`, `crates/agentprism-ai/src/streaming.rs:1708-1896` | shared | excluded | Internal protocol reducer used by core and the Tokio mirror. |

### `Models` control plane

`Models` is a cloneable provider/model/auth control-plane handle whose inner
state owns provider registrations, credentials, auth context, catalog stores,
and middleware (`crates/agentprism-ai/src/models.rs:41-63`).

| Item group | Current source | Class | Role |
|---|---|---:|---|
| `Models`, `Models::builder`, `ModelsBuilder` | `crates/agentprism-ai/src/models.rs:47-51`, `crates/agentprism-ai/src/models.rs:107-111`, `crates/agentprism-ai/src/models.rs:1444-1455` | core object; extended builder | Concrete control plane and current Rust composition entry point. |
| `stream_simple` | `crates/agentprism-ai/src/models.rs:760-768` | core | Provider-neutral direct model execution. |
| `stream_simple_with_auth` | `crates/agentprism-ai/src/models.rs:770-780` | extended | Same direct path with explicit request-scoped auth overrides. |
| `stream_api`, `stream_api_with_request_options` | `crates/agentprism-ai/src/models.rs:783-822` | extended | Generic `ApiFamily` execution with associated option types. |
| providers/model queries | `crates/agentprism-ai/src/models.rs:113-163`, `crates/agentprism-ai/src/models.rs:247-256` | extended | Owned provider/model snapshots and lookup. |
| auth/availability | `crates/agentprism-ai/src/models.rs:165-367` | extended | `check_auth`, `get_available`, credential metadata, auth resolution, login, and logout. |
| catalog mutation/refresh | `crates/agentprism-ai/src/models.rs:369-544` | extended | Catalog snapshots/layers, provider mutation, overrides, and refresh. |
| deferred fetch/cancel | `crates/agentprism-ai/src/models.rs:824-927` | extended | Redeem or cancel durable provider work. |
| builder stores/provider/middleware | `crates/agentprism-ai/src/models.rs:1473-1541` | excluded authoring | Accepts numerous `Arc<dyn ...>` values plus generic `PayloadTransform<A>`. |
| provider registration builders | `crates/agentprism-ai/src/provider.rs:2320-2527` | excluded authoring | Provider descriptors, auth, catalogs, APIs, filters, and retry callbacks. |
| `ScriptedRuntime` | `crates/agentprism-ai/src/scripted.rs:323-389` | excluded fixture | Deterministic test runtime used by core/runtime tests, not production construction. |

`ModelsBuilder` currently defaults to in-memory credentials, empty auth
context, and in-memory catalog stores
(`crates/agentprism-ai/src/models.rs:1457-1469`). A provider-neutral native
factory/runtime owner is not present in these current shapes; Design must
address construction without turning the generic provider-authoring graph into
the basic consumer API.

### Canonical message, tool, replay, accounting, and request-option graph

The table follows field containment from the scoped roots. Optional values are
not demoted: `ModelRequest::options`, `Context::tools`, and
`ToolResultMessage::details` are still part of the ordinary owned contract when
their optional branches are populated.

| Value group | Why it is in this class | Current source | Class |
|---|---|---|---:|
| `Message`, `UserMessage`, `AssistantMessage`, `ToolResultMessage` | Canonical durable transcript variants nested in `AgentRecord` and `Context`. | `crates/agentprism-ai/src/messages.rs:26-170`; `crates/agentprism-ai/src/messages.rs:220-242` | core |
| `DiagnosticErrorCode`, `DiagnosticErrorInfo`, `AssistantMessageDiagnostic` | Nested in `AssistantMessage`, `AssistantMessageSnapshot`, and `AssistantEvent::DiagnosticAdded`. | `crates/agentprism-ai/src/messages.rs:172-218`; `crates/agentprism-ai/src/streaming.rs:426-431` | core |
| `ContentBlock`, `ToolCall` | Nested in messages, snapshots, and agent/tool lifecycle events. | `crates/agentprism-ai/src/messages.rs:244-307`; `crates/agentprism-core/src/events.rs:112-141` | core |
| `ToolSpec` | `Context::tools` is a direct field of the `ModelRequest::context` root. | `crates/agentprism-ai/src/messages.rs:309-326`; `crates/agentprism-ai/src/messages.rs:464-488`; `crates/agentprism-ai/src/runtime.rs:11-20` | core |
| `ConstrainedSampling`, `ConstrainedSamplingConfig`, `JsonSchemaStrictMode`, `GrammarFormat`, `GrammarVariants` | Direct/transitive optional values under core `ToolSpec::constrained_sampling`; optionality does not change their class. | `crates/agentprism-ai/src/messages.rs:320-418` | core |
| `ToolResultContent`, `Context` | Tool-result blocks are nested in event/transcript values; `Context` is a direct `ModelRequest` field. | `crates/agentprism-ai/src/messages.rs:420-440`; `crates/agentprism-ai/src/messages.rs:464-488`; `crates/agentprism-ai/src/runtime.rs:13-20` | core |
| `Conversation` | Standalone durable conversation convenience; the Agent state itself uses `AgentRecord`. | `crates/agentprism-ai/src/messages.rs:442-462` | extended |
| `AssistantFinish`, `AssistantFinishReason`, `PublicError` | Terminal assistant, turn, and run status graph. | `crates/agentprism-ai/src/messages.rs:490-534`; `crates/agentprism-core/src/events.rs:26-75` | core |
| IDs, `ModelRef`, `Timestamp` | Open string identifiers and Unix-millisecond time embedded throughout requests, events, outcomes, replay, and snapshots. | `crates/agentprism-ai/src/ids.rs:58-120`; `crates/agentprism-ai/src/ids.rs:128-147` | core |
| `ReplayEnvelope` graph | Scope, items, targets, applicability, completeness, and opaque payloads nested in assistant values and events. | `crates/agentprism-ai/src/replay.rs:13-371` | core |
| handoff report graph | `ModelFingerprint`, replay-drop reasons, changes, and `HandoffReport` carried by `AgentEvent::ContextPrepared`. | `crates/agentprism-ai/src/handoff.rs:17-159`; `crates/agentprism-core/src/events.rs:96-104` | core |
| `Usage`, `Cost`, `Currency` | Token/cost values carried by messages, stream events, turn/run outcomes, snapshots, and tool output. | `crates/agentprism-ai/src/usage.rs:13-140` | core |
| `ReasoningLevel`, `SimpleGenerationOptions` | `ReasoningLevel` is in `AgentState`; `SimpleGenerationOptions` is the direct `ModelRequest::options` field. | `crates/agentprism-core/src/state.rs:18-34`; `crates/agentprism-ai/src/options.rs:13-36`; `crates/agentprism-ai/src/options.rs:554-612`; `crates/agentprism-ai/src/runtime.rs:11-20` | core |
| `ReasoningFallback`, `ThinkingBudgets` | Direct fields `reasoning_fallback` and `thinking_budgets` of core `SimpleGenerationOptions`. | `crates/agentprism-ai/src/options.rs:73-125`; `crates/agentprism-ai/src/options.rs:582-592` | core |
| `StreamTransport`, `CacheRetention`, `ToolChoice` | Direct optional fields `transport`, `cache_retention`, and `tool_choice` of core `SimpleGenerationOptions`. | `crates/agentprism-ai/src/options.rs:133-155`; `crates/agentprism-ai/src/options.rs:504-520`; `crates/agentprism-ai/src/options.rs:567-606` | core |
| `DeferredSubmission`, `DeferredWindow` | `DeferredSubmission` is the direct optional `SimpleGenerationOptions::deferred` field and recursively contains `DeferredWindow`. | `crates/agentprism-ai/src/deferred.rs:100-173`; `crates/agentprism-ai/src/options.rs:607-609` | core |
| `ErasedApiOptionsPatch` | Direct optional `SimpleGenerationOptions::api_options` value; contains `ApiId`, schema version, and exact raw JSON. | `crates/agentprism-ai/src/options.rs:291-309`; `crates/agentprism-ai/src/options.rs:610-611` | core |
| `OrderedJsonObject`, `OrderedJsonValue`, `OrderedJsonArray`, `OrderedJsonString` | `SimpleGenerationOptions::sampling` directly stores `OrderedJsonObject`; its key/value graph is recursive. | `crates/agentprism-ai/src/options.rs:593-598`; `crates/agentprism-ai/src/json_compat.rs:16-24`; `crates/agentprism-ai/src/json_compat.rs:108-114`; `crates/agentprism-ai/src/json_compat.rs:225-227`; `crates/agentprism-ai/src/json_compat.rs:318-340` | core |
| `HeaderMapSpec` | Direct `SimpleGenerationOptions::headers` field; canonical alias is `BTreeMap<String, Option<String>>`. | `crates/agentprism-ai/src/model.rs:15-17`; `crates/agentprism-ai/src/options.rs:599-602` | core |
| `VersionedExtension` | Direct optional `ToolResultMessage::details` value; stores schema version and exact raw JSON. | `crates/agentprism-ai/src/messages.rs:220-242`; `crates/agentprism-ai/src/model.rs:917-930` | core |

`ApiRequestOptions` and the typed/erased API-family lowering inputs are
**extended** or **excluded authoring** depending on the method that uses them:
the concrete transport/options record is used by the extended fully typed and
deferred `Models` paths (`crates/agentprism-ai/src/options.rs:443-481`), while
`ApiFamily`, `TypedModelDescriptor<A>`, borrowed lowering contexts,
`ApiOptionsInput<A>`, and `ErasedApiFullOptions` are generic provider/lowering
composition seams (`crates/agentprism-ai/src/options.rs:311-441` and
`crates/agentprism-ai/src/options.rs:522-552`) and are excluded from the
ordinary concrete consumer boundary.

### Cancellation and deferred values

`CancellationToken` is a cloneable executor-neutral capability containing
shared atomic/mutex state (`crates/agentprism-ai/src/cancellation.rs:22-37`).
Its owned/synchronous surface is `new`, `cancel`, `is_cancelled`, `check`, and
`child` (`crates/agentprism-ai/src/cancellation.rs:45-102`) and is **core**.
`CancellationError` is likewise **core** because it is the concrete error in
the retained `CancellationToken::check` result
(`crates/agentprism-ai/src/cancellation.rs:10-20` and
`crates/agentprism-ai/src/cancellation.rs:71-78`).
`cancelled()` returns the borrowing `Cancelled<'_>` future at
`crates/agentprism-ai/src/cancellation.rs:80-85` and
`crates/agentprism-ai/src/cancellation.rs:113-161`; that future carrier is
**excluded**, while cancellation itself remains core.

`DeferredHandle` is owned durable data containing provider/model/API identity,
provider token, expiry, polling hint, and provider JSON
(`crates/agentprism-ai/src/deferred.rs:11-69`). It is **core transitively**
because `AssistantMessage` can contain it. `DeferredSubmission` and its nested
`DeferredWindow` are also **core**, because the core
`SimpleGenerationOptions` record contains the submission preference directly
(`crates/agentprism-ai/src/deferred.rs:100-173` and
`crates/agentprism-ai/src/options.rs:607-609`). `DeferredCapabilities`,
`DeferredFetchOptions`, and `DeferredCancelOptions` are **extended** values for
the optional deferred control-plane operations
(`crates/agentprism-ai/src/deferred.rs:71-98` and
`crates/agentprism-ai/src/deferred.rs:175-188`). `DeferredModelRuntime` is an
optional trait-object execution seam at
`crates/agentprism-ai/src/deferred.rs:190-215`; direct `Models` methods are the
consumer-facing extended path.

## Excluded surfaces

The exclusions are intentional scope boundaries, not inferred FFI limits:

| Surface | Current evidence | Reason |
|---|---|---|
| `agentprism-harness` and the session/environment/compaction/skills/orchestration APIs it composes, including the Tokio environment implementations re-exported by the runtime crate | crate exports at `crates/agentprism-harness/src/lib.rs:9-40`; Tokio environment re-exports at `crates/agentprism-runtime-tokio/src/lib.rs:9-14` | Owner decision: the production coding-agent SDK is a future milestone. This is the only harness-surface listing in this inventory. |
| Local execution family | `LocalModelRuntime` at `crates/agentprism-ai/src/runtime.rs:96-107`, `LocalAssistantStream` at `crates/agentprism-ai/src/streaming.rs:1963-2015`, and `LocalAgent` at `crates/agentprism-core/src/restore.rs:220-240` | Single-threaded/WASM twin, not the Tokio/Send SDK. |
| Bare Agent run/recovery streams | `crates/agentprism-core/src/run.rs:374-495` | Borrow `&mut Agent` and expose boxed stream/lifetime/recovery machinery. |
| Scheduler and policy authoring | `crates/agentprism-core/src/scheduler.rs:127-269`, `crates/agentprism-core/src/policy.rs:116-522` | Generic, borrowed, and host-authored extension seams. |
| Tool/provider/middleware/auth-store authoring | `crates/agentprism-core/src/tools.rs:183-550`, `crates/agentprism-ai/src/provider.rs:2320-2527`, `crates/agentprism-ai/src/models.rs:1473-1541` | Separate callback-authoring/construction work; values they emit remain in scope. |
| Raw Tokio receivers | `crates/agentprism-runtime-tokio/src/lib.rs:131-140`, `crates/agentprism-runtime-tokio/src/lib.rs:368-371` | Executor-specific low-level access behind otherwise concrete actor values. |
| Scripted and deterministic auth fixtures | `crates/agentprism-ai/src/scripted.rs:323-389`; existing C example config at `bindings/agentprism-ffi/examples/scripted_host.rs:48-72` | Test-only construction rather than the production model/provider path. |

## Explicit Rust-side FFI-hard spots

This section describes Rust shapes only.

| Hard spot | Current code evidence | Boundary consequence |
|---|---|---|
| Borrowed agent streams | `Agent::{run,prompt_text,prompt_records,continue_run,retry_last_turn}` borrow `&mut self` and return `SendBoxStream<'a, AgentEvent>` at `crates/agentprism-core/src/run.rs:374-446`. | Keep behind the owned Tokio actor. |
| Mutable/consuming run handle | `TokioAgentRun::next_event(&mut self)` and `outcome(self)` are at `crates/agentprism-runtime-tokio/src/lib.rs:137-145`. | The current concrete class does not yet have shareable `&self` pull/outcome methods or reusable completion. |
| Run-establishment handoff | `request_run` waits on a consuming acceptance oneshot before constructing `TokioAgentRun` at `crates/agentprism-runtime-tokio/src/lib.rs:394-415`. | Dropping the establishment future or returned run crosses actor-owned resource lifetime. No run lease/drop policy is present in the shown type. |
| Bounded observation plus sink barrier | Every run owns an `mpsc::Sender<AgentEvent>` at `crates/agentprism-runtime-tokio/src/lib.rs:419-424`; `dispatch_event` awaits that send before sinks at `crates/agentprism-runtime-tokio/src/lib.rs:832-853`. | Sink-only and abandoned-observation behavior must be explicit. |
| Callback future trait | `AgentEventSink::on_event` returns `SendBoxFuture<'static, ()>` at `crates/agentprism-runtime-tokio/src/lib.rs:45-51`. | Host callback acknowledgement is part of producer ordering, not mere observation. |
| Runtime acquisition | `TokioAgentHandle::with_capacities` uses `tokio::runtime::Handle::try_current()` and spawns without retaining an owner at `crates/agentprism-runtime-tokio/src/lib.rs:177-200`. | Current construction assumes an ambient runtime. |
| Direct assistant stream | `Models::stream_simple` returns a future containing `AssistantStream` at `crates/agentprism-ai/src/models.rs:760-780`; `AssistantStream` is a Rust `Stream` at `crates/agentprism-ai/src/streaming.rs:1934-1955`. | A concrete Rust-owned pull/lifecycle object is absent today. |
| Trait-object assembly | `Agent::new` takes `Arc<dyn ModelRuntime>` and `ToolRegistry` at `crates/agentprism-core/src/run.rs:217-221`; `ModelsBuilder` accepts multiple trait objects at `crates/agentprism-ai/src/models.rs:1473-1541`. | A normal consumer factory must preserve canonical ownership without exposing the authoring graph as basic setup. |
| Generic provider/API methods | `Models::stream_api<A: ApiFamily>` uses `A::FullOptions` at `crates/agentprism-ai/src/models.rs:783-822`; `ApiFamily` has five associated types at `crates/agentprism-ai/src/options.rs:368-405`. | Concrete API-family entry points differ from the provider-neutral direct path. |
| Heterogeneous enums and recursive/opaque data | `AgentEvent` at `crates/agentprism-core/src/events.rs:77-155`, `AssistantEvent` at `crates/agentprism-ai/src/streaming.rs:356-538`, recursive ordered JSON at `crates/agentprism-ai/src/json_compat.rs:16-360`, and raw/custom JSON at `crates/agentprism-core/src/state.rs:62-71`. | The boundary must retain the canonical owned hierarchy and payload fidelity. |
| Nested auth callbacks | `Models::login` accepts `Arc<dyn AuthInteraction>` at `crates/agentprism-ai/src/models.rs:311-345`; auth interaction and redirect receiver traits are at `crates/agentprism-ai/src/auth.rs:1085-1115` and `crates/agentprism-ai/src/auth.rs:1207-1222`. | Interactive login is a separate callback graph within the extended control plane. |
| Envelope mismatch | `AgentEventEnvelope` exists at `crates/agentprism-core/src/events.rs:327-336`, but runtime `RunChannels`, `TokioAgentRun`, and `dispatch_event` carry bare `AgentEvent` at `crates/agentprism-runtime-tokio/src/lib.rs:120-145`, `crates/agentprism-runtime-tokio/src/lib.rs:419-424`, and `crates/agentprism-runtime-tokio/src/lib.rs:832-853`. | The current actor has no single authoritative envelope allocation/fan-out point. |

## Audit-support appendix: exact current actor code

The snippets below are copied from the current tree so Design can address audit
findings 2-8 and 16 against ground truth.

### A. `TokioAgentRun` fields and methods

Source: `crates/agentprism-runtime-tokio/src/lib.rs:120-146`.

```rust
/// One accepted actor-owned run.
///
/// Events are observational and delivered over a bounded channel. A caller
/// that retains this value should drain [`Self::events`] or call
/// [`Self::next_event`] while the run is active. Registered event sinks, rather
/// than this receiver, provide the explicit acknowledgement barrier contract.
pub struct TokioAgentRun {
    events: mpsc::Receiver<AgentEvent>,
    completion: oneshot::Receiver<Result<RunOutcome, TokioAgentError>>,
}

impl TokioAgentRun {
    /// Returns the bounded ordered event receiver for this run.
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events
    }

    /// Receives the next observational event.
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// Waits for the run and its `RunFinished` sink barriers to settle.
    pub async fn outcome(self) -> Result<RunOutcome, TokioAgentError> {
        self.completion.await.map_err(|_| TokioAgentError::Closed)?
    }
}
```

The type has no cancellation token, completion cache, observation-state flag,
or explicit drop policy in this current definition.

### B. Run request handoff and channel ownership

Source: `crates/agentprism-runtime-tokio/src/lib.rs:394-424`.

```rust
async fn request_run(
    &self,
    sink: Option<Arc<dyn AgentEventSink>>,
    command: impl FnOnce(RunChannels) -> AgentCommand,
) -> Result<TokioAgentRun, TokioAgentError> {
    let (events_tx, events) = mpsc::channel(self.event_capacity);
    let (completion, completion_rx) = oneshot::channel();
    let (accepted, accepted_rx) = oneshot::channel();
    self.command_tx
        .send(command(RunChannels {
            events: events_tx,
            completion,
            accepted: Some(accepted),
            sink,
        }))
        .await
        .map_err(|_| TokioAgentError::Closed)?;
    accepted_rx.await.map_err(|_| TokioAgentError::Closed)??;
    Ok(TokioAgentRun {
        events,
        completion: completion_rx,
    })
}
}

struct RunChannels {
    events: mpsc::Sender<AgentEvent>,
    completion: oneshot::Sender<Result<RunOutcome, TokioAgentError>>,
    accepted: Option<oneshot::Sender<Result<(), TokioAgentError>>>,
    sink: Option<Arc<dyn AgentEventSink>>,
}
```

The standalone `}` after `request_run` closes the `impl TokioAgentHandle`
block in the source.

### C. `accept_run` and idle restoration

Source: `crates/agentprism-runtime-tokio/src/lib.rs:699-708`.

```rust
fn accept_run(idle_tx: &watch::Sender<bool>, channels: &mut RunChannels) -> bool {
    let Some(accepted) = channels.accepted.take() else {
        return false;
    };
    if accepted.is_closed() {
        return false;
    }
    let _ = idle_tx.send(false);
    accepted.send(Ok(())).is_ok()
}
```

Current normal completion restores idle here, after `drive_run` returns.
Source: `crates/agentprism-runtime-tokio/src/lib.rs:720-734`.

```rust
fn finish_owned_run(
    agent: &Agent,
    state_tx: &watch::Sender<AgentSnapshot>,
    idle_tx: &watch::Sender<bool>,
    completion: oneshot::Sender<Result<RunOutcome, TokioAgentError>>,
    result: DriveResult,
) -> bool {
    let _ = state_tx.send(agent.snapshot());
    let _ = idle_tx.send(true);
    let _ = completion.send(result.outcome);
    for response in result.shutdown_responses {
        let _ = response.send(());
    }
    result.shutdown_requested
}
```

The observer waits on the boolean watch using this loop. Source:
`crates/agentprism-runtime-tokio/src/lib.rs:373-382`.

```rust
pub async fn wait_for_idle(&self) -> Result<(), TokioAgentError> {
    let mut idle = self.idle_rx.clone();
    loop {
        if *idle.borrow_and_update() {
            return Ok(());
        }
        idle.changed().await.map_err(|_| TokioAgentError::Closed)?;
    }
}
```

### D. `prompt_text_with_sink` still installs pull observation

Source: `crates/agentprism-runtime-tokio/src/lib.rs:211-227`.

```rust
/// Starts a run with an acknowledged sink scoped to that accepted run.
///
/// The sink and prompt are submitted as one actor command. If another run
/// is active, the command is rejected without registering or invoking the
/// sink. An accepted sink observes only the events from the run created by
/// this command and is removed automatically when that run settles.
pub async fn prompt_text_with_sink(
    &self,
    prompt: PromptText,
    sink: Arc<dyn AgentEventSink>,
) -> Result<TokioAgentRun, TokioAgentError> {
    self.request_run(Some(sink), |channels| AgentCommand::PromptText {
        prompt,
        channels,
    })
    .await
}
```

Because this calls the single `request_run` shown in appendix B, the current
method means sink **plus** pull observation. It always creates the bounded event
channel and returns its receiver.

### E. Current fan-out ordering and actor completion validation

Source: `crates/agentprism-runtime-tokio/src/lib.rs:736-774` and
`crates/agentprism-runtime-tokio/src/lib.rs:823-830`.

```rust
async fn drive_run<'a>(
    mut stream: agentprism_ai::SendBoxStream<'a, AgentEvent>,
    cancellation: CancellationToken,
    mut snapshot: AgentSnapshot,
    events: mpsc::Sender<AgentEvent>,
    context: DriveContext<'_>,
) -> DriveResult {
    let mut assembler = None::<AssistantAssembler>;
    let mut outcome = None;
    let mut shutdown_responses = Vec::new();
    let mut shutdown_requested = false;
    let mut commands_open = true;
    let mut events = Some(events);

    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else {
                    break;
                };
                if let Err(error) = apply_event_to_snapshot(&mut snapshot, &mut assembler, &event) {
                    return DriveResult {
                        outcome: Err(error),
                        shutdown_responses,
                        shutdown_requested,
                    };
                }
                let _ = context.state_tx.send(snapshot.clone());
                if let AgentEvent::RunFinished { outcome: run_outcome } = &event {
                    outcome = Some(run_outcome.clone());
                }
                dispatch_event(
                    &mut events,
                    event,
                    context.sinks,
                    context.run_sink.as_ref(),
                    cancellation.clone(),
                )
                .await;
            }
```

```rust
    DriveResult {
        outcome: outcome.ok_or(TokioAgentError::MissingRunFinished),
        shutdown_responses,
        shutdown_requested,
    }
}
```

Source: `crates/agentprism-runtime-tokio/src/lib.rs:832-853`.

```rust
async fn dispatch_event(
    events: &mut Option<mpsc::Sender<AgentEvent>>,
    event: AgentEvent,
    sinks: &[RegisteredSink],
    run_sink: Option<&Arc<dyn AgentEventSink>>,
    cancellation: CancellationToken,
) {
    if let Some(sender) = events
        && sender.send(event.clone()).await.is_err()
    {
        *events = None;
    }
    for registered in sinks {
        registered
            .sink
            .on_event(event.clone(), cancellation.clone())
            .await;
    }
    if let Some(run_sink) = run_sink {
        run_sink.on_event(event, cancellation).await;
    }
}
```

### F. Envelope and sequence ground truth: no actor envelope allocation

The canonical envelope currently exists only as this value. Source:
`crates/agentprism-core/src/events.rs:327-336`.

```rust
/// Monotonically sequenced event envelope used for persistence and FFI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    /// Global event sequence allocated by this agent instance.
    pub sequence: u64,
    /// Run to which this event belongs.
    pub run_id: RunId,
    /// Ordered state-machine event.
    pub event: AgentEvent,
}
```

The Tokio actor does not construct that type. `drive_run` and `dispatch_event`
in appendix E carry bare `AgentEvent`. Its mirror sequence is updated inside
snapshot application before variant validation. Source:
`crates/agentprism-runtime-tokio/src/lib.rs:855-864`.

```rust
fn apply_event_to_snapshot(
    snapshot: &mut AgentSnapshot,
    assembler: &mut Option<AssistantAssembler>,
    event: &AgentEvent,
) -> Result<(), TokioAgentError> {
    snapshot.next_sequence = snapshot.next_sequence.checked_add(1).ok_or_else(|| {
        TokioAgentError::SnapshotInvariant {
            message: "agent snapshot event sequence overflowed".into(),
        }
    })?;
```

Validation can then return `SnapshotInvariant`; for example, assistant
identity validation begins at
`crates/agentprism-runtime-tokio/src/lib.rs:866-919`, and the overlapping
assistant lifecycle check is at
`crates/agentprism-runtime-tokio/src/lib.rs:950-960`. Thus the actor's current
observable channel has no envelope allocation, while the mirrored
`next_sequence` may already have advanced before a later validation error.

The core borrowed stream derives run identity from the current global event
sequence before it starts yielding. Source:
`crates/agentprism-core/src/run.rs:546-571`.

```rust
fn start_run_with_context<'a>(
    &'a mut self,
    records: Vec<AgentRecord>,
    poll_initial_steering: bool,
    cancellation: CancellationToken,
    initial_context_records: Option<Vec<AgentRecord>>,
    recovery: Option<CompletedTurnRecoveryStream<'a>>,
    recovered_run_state: Option<RecoveredRunState>,
) -> SendBoxStream<'a, AgentEvent> {
    self.require_idle()
        .expect("a borrowed Agent cannot safely start a second active run");
    let run_id = RunId::new(format!("agent-run-{}", self.next_sequence));
    let cancellation = cancellation.child();
    self.queue_rx
        .register_run(run_id.clone(), cancellation.clone())
        .expect("an open idle agent must accept its run registration");
    self.active_run = Some(run_id.clone());
    self.last_error = None;
    self.streaming = None;
    self.pending_tool_calls = Arc::from([]);
    let guard = SendRunGuard {
        agent: self,
        run_id,
        cancellation,
        finished: false,
    };
```

It advances that core counter per emitted bare event through this method.
Source: `crates/agentprism-core/src/run.rs:2174-2176`.

```rust
fn bump_event_sequence(&mut self) {
    self.next_sequence = self.next_sequence.saturating_add(1);
}
```

Neither core run path nor the Tokio mirror constructs
`AgentEventEnvelope`: core yields `SendBoxStream<'a, AgentEvent>` above, and
appendices B and E show that the actor channels and fan-out also carry bare
`AgentEvent`.

### G. `TokioAgentHandle::new` runtime acquisition

Source: `crates/agentprism-runtime-tokio/src/lib.rs:166-200`.

```rust
impl TokioAgentHandle {
    /// Starts an owner task using the default bounded capacities.
    pub fn new(agent: Agent) -> Result<Self, TokioAgentError> {
        Self::with_capacities(agent, DEFAULT_COMMAND_CAPACITY, DEFAULT_EVENT_CAPACITY)
    }

    /// Alias for [`Self::new`] emphasizing that construction starts the actor.
    pub fn spawn(agent: Agent) -> Result<Self, TokioAgentError> {
        Self::new(agent)
    }

    /// Starts an owner task with explicit bounded channel capacities.
    pub fn with_capacities(
        agent: Agent,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, TokioAgentError> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| TokioAgentError::NoRuntime)?;
        let command_capacity = command_capacity.max(1);
        let event_capacity = event_capacity.max(1);
        let snapshot = agent.snapshot();
        let direct_control = agent.control();
        let (command_tx, command_rx) = mpsc::channel(command_capacity);
        let (state_tx, state_rx) = watch::channel(snapshot);
        let (idle_tx, idle_rx) = watch::channel(true);
        runtime.spawn(actor_loop(agent, command_rx, state_tx, idle_tx));
        Ok(Self {
            command_tx,
            state_rx,
            idle_rx,
            direct_control,
            event_capacity,
        })
    }
```

The handle stores no runtime owner or runtime lease in its fields
(`crates/agentprism-runtime-tokio/src/lib.rs:157-164`). `Models` remains a
separate control-plane object (`crates/agentprism-ai/src/models.rs:47-63`), and
`Agent::new` still requires explicit `ModelRuntime` and `ToolRegistry`
injection (`crates/agentprism-core/src/run.rs:217-221`). These are the current
facts Design must use when separating runtime ownership from agent/model
assembly.

## Resulting scoped boundary

The smallest current-tree contract set for this milestone is:

1. A Rust-owned Tokio lifetime and a legitimate canonical construction path
   that can assemble `Models`/`ModelRuntime`, `AgentState`, internal tools, and
   `TokioAgentHandle` without exposing generic provider/tool authoring as basic
   setup.
2. `TokioAgentHandle`, a reshaped owned `TokioAgentRun`, prompt/continue/retry,
   steering/follow-up/cancellation, reset, owned snapshots, idle, and shutdown.
3. Ordered, lossless pull delivery of the canonical `AgentEvent` graph, with
   the envelope decision made once in the canonical runtime rather than in a
   separate binding record hierarchy.
4. `AgentEventSink` only for acknowledged callback semantics, preserving its
   producer-barrier ordering and its distinction from pull observation.
5. `Models` and a Rust-owned direct assistant pull/lifecycle object over the
   existing `AssistantStream` and canonical `AssistantEvent` graph.
6. The complete owned state, message, content, replay, usage/cost, tool-event,
   cancellation, outcome, error, and deferred value graph transitively required
   by those operations.

The current gaps exposed by inventory are concrete: run acceptance can leave
idle false on the check-to-send race; the returned run has no drop/cancellation
lease; completion is consuming; `outcome` does not close/drain observation;
sink-plus-pull is the only sink-prompt shape; producer validation and consumer
delivery state are not separate; the actor emits no canonical envelopes;
runtime acquisition is ambient; direct model streaming has no owned Tokio pull
facade; and runtime ownership is not separated from application assembly.
These are Design inputs, not new binding-layer contracts.
