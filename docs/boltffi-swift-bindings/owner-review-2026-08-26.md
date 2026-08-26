# Owner review — ADOPTED (owner decision, 2026-08-26)

The owner adopted this review as the authoritative statement of intent for the BoltFFI Swift
bindings design, superseding the prior R2 requirement ("no code changes beyond attributes"),
which was an over-prescription introduced by the workflow author, not the owner's goal. The
design must address every section below. Where this review makes claims about BoltFFI, they
must be re-verified against the documentation snapshot and cited; where it makes claims about
this repository's code, they must be verified at file:line.

---

## Feedback on the BoltFFI Swift design workflow

The BoltFFI documentation research is thorough, but the workflow imposed a requirement that prevents it from producing the intended design:

> R2 permits no crate changes beyond attributes on existing items.

That is stricter than the actual goal. The goal is not “export every existing Rust signature without changing it.” The goal is:

- No UDL or separate IDL.
- No separately maintained binding crate or duplicate Rust facade.
- No required handwritten Swift wrapper.
- Export the canonical concrete Rust consumer API inline.
- Permit small canonical Rust API improvements—owned returns, concrete overloads, interior synchronization, and async pull methods—when required for a sound cross-language API.
- Keep Rust-only provider-authoring, generic, borrowed, and low-level executor seams unannotated.

Please revise the workflow and design under that requirement.

A better R2 would be:

> Integration must not introduce a separately maintained FFI facade, duplicate record hierarchy, IDL, or required Swift wrapper. Existing canonical crates may receive inline BoltFFI annotations and minimal concrete API changes needed to project their ordinary consumer contracts safely, including owned returns, concrete collection inputs, interior synchronization, async pull methods, and Rust-owned runtime integration. Every such change must remain a legitimate Rust API rather than a binding-only command/envelope layer.

## Core streaming fact

BoltFFI’s documented `#[ffi_stream]` method must return:

```rust
Arc<EventSubscription<T>>
```

The documentation explicitly says that each subscription has a finite ring buffer and:

> When the buffer is full, new events are dropped. The producer continues without blocking.

Therefore `EventSubscription<AssistantEvent>` and `EventSubscription<AgentEvent>` are unsuitable for authoritative model and agent streams. Increasing capacity reduces probability but does not change the contract.

The appropriate alternative is an ordinary exported async pull method on a Rust-owned class:

```rust
pub async fn next_event(&self) -> ...
```

BoltFFI generates a normal Swift async method. It does not generate `AsyncStream`, but semantic correctness is more important than `for await` syntax:

```swift
while let event = try await run.nextEvent() {
    // ...
}
```

This approach keeps event ownership in Rust, introduces no second queue, and allows Rust to retain backpressure.

## Apply this to the Agent runtime, not the bare `Agent`

The ordinary Swift boundary should be the concrete Tokio actor facade:

- `TokioAgentHandle`
- `TokioAgentRun`
- Owned `AgentEvent`/outcome/snapshot/control values
- `AgentEventSink` only when acknowledged callback semantics are needed

Do not try to export the low-level methods in `pi-agent-core::Agent`:

```rust
Agent::run
Agent::prompt_text
Agent::prompt_records
Agent::continue_run
Agent::retry_last_turn
```

Those return streams borrowing `&mut Agent`. They are intentional Rust composition seams. The concrete application-facing actor already exists in `pi-agent-runtime-tokio`.

## Current Agent delivery path

The latest code has this path:

```text
Agent::run borrowed stream
          │
          │ actor polls it
          ▼
bounded Tokio mpsc channel
          │
          │ sender.send(event).await
          ▼
TokioAgentRun::next_event
          │
          ▼
consumer
```

Relevant symbols:

- `crates/pi-agent-core/src/run.rs`: `Agent::run`
- `crates/pi-agent-runtime-tokio/src/lib.rs`: `TokioAgentRun`
- `crates/pi-agent-runtime-tokio/src/lib.rs`: `drive_run`
- `crates/pi-agent-runtime-tokio/src/lib.rs`: `dispatch_event`

The Tokio channel is bounded, but it is not lossy:

```rust
sender.send(event.clone()).await
```

When full, the actor waits. It does not discard the event.

Do not add a BoltFFI subscription after it:

```text
lossless Tokio mpsc
        ↓
drop-on-full Bolt ring
        ↓
Swift AsyncStream
```

That would introduce loss only at the foreign-language boundary.

## Reshape `TokioAgentRun`

The current type is:

```rust
pub struct TokioAgentRun {
    events: mpsc::Receiver<AgentEvent>,
    completion: oneshot::Receiver<Result<RunOutcome, TokioAgentError>>,
}

impl TokioAgentRun {
    pub async fn next_event(&mut self) -> Option<AgentEvent>;

    pub async fn outcome(self)
        -> Result<RunOutcome, TokioAgentError>;
}
```

This is not the right final BoltFFI shape:

1. `next_event(&mut self)` requires unsafe `#[export(single_threaded)]`.
2. `outcome(self)` consumes the class.
3. Current BoltFFI source explicitly rejects owned class receivers because its generated foreign handle cannot be safely invalidated after the Rust value is consumed.
4. Event-channel EOF currently hides actor failures unless the caller separately awaits `outcome`.
5. Calling `outcome()` without draining a run that produces more events than channel capacity can deadlock: the actor waits for event-channel capacity while `outcome` waits for actor completion.

The canonical type should use interior synchronization:

```rust
pub struct TokioAgentRun {
    events: tokio::sync::Mutex<mpsc::Receiver<AgentEvent>>,
    completion:
        watch::Receiver<Option<Result<RunOutcome, TokioAgentError>>>,
    cancellation: CancellationToken,
}
```

The precise implementation may differ, but the exported API should have this shape:

```rust
#[boltffi::export]
impl TokioAgentRun {
    pub async fn next_event(
        &self,
    ) -> Result<Option<AgentEvent>, TokioAgentError>;

    pub async fn outcome(
        &self,
    ) -> Result<RunOutcome, TokioAgentError>;

    pub fn cancel(&self);
}
```

### `next_event`

`next_event` should:

1. Serialize access to the receiver.
2. Receive one event.
3. Return it unchanged.
4. On channel EOF, inspect the cached actor completion.
5. Return `Ok(None)` only for validated normal stream completion.
6. Return `MissingRunFinished`, `SnapshotInvariant`, or `Closed` as an error when appropriate.

This generates:

```swift
while let event = try await run.nextEvent() {
    consume(event)
}
```

Expected provider failure and cancellation remain in-band as `RunOutcome::Failed` and `RunOutcome::Cancelled`. Only actor/protocol failures throw.

The class remains `Send + Sync`; `#[export(single_threaded)]` is unnecessary.

Concurrent `next_event` calls should either be serialized or rejected explicitly as concurrent polling. Do not leave synchronization responsibility to Swift.

### `outcome`

`outcome(&self)` must preserve the practical semantics of the old consuming method:

> Calling `outcome` means the caller no longer intends to consume observational events.

It should:

1. Close the observational receiver.
2. Discard already-buffered observations.
3. Wake any actor send blocked on channel capacity.
4. Await a reusable/cached completion result.

A `watch`-style completion value is preferable to a consuming `oneshot` because cancellation of one Swift await must not destroy the only completion receiver.

After `outcome()` begins, later `next_event()` calls should return end-of-stream or a documented terminal state.

### Keep raw receiver access Rust-only

If Rust callers still need:

```rust
pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent>
```

keep it in a separate unannotated inherent impl. Annotate only the concrete cross-language impl block. This preserves advanced Rust access without forcing Tokio receiver types through BoltFFI.

## Cancellation semantics

Cancelling a Swift task awaiting:

```swift
await run.nextEvent()
```

only cancels that one Rust future. It does not cancel the agent run.

Tokio’s bounded `Receiver::recv` is cancellation-safe, so cancelling the wait does not consume the next event. But if Swift never resumes:

1. The event channel eventually fills.
2. The actor blocks in `send().await`.
3. Mailbox cancellation, steering, follow-up, sink invocation, and completion can stall.

`TokioAgentRun` should therefore retain the run cancellation token and expose:

```rust
pub fn cancel(&self) {
    self.cancellation.cancel();
}
```

A useful canonical convenience is:

```rust
pub async fn cancel_and_outcome(
    &self,
) -> Result<RunOutcome, TokioAgentError>;
```

It should cancel work, close/discard the observational stream, and await terminal settlement.

Swift callers that want the committed cancellation lifecycle should instead call `cancel()` and continue draining until `RunFinished::Cancelled`.

## Do not conflate pull events with acknowledged sinks

`AgentEventSink` has stronger semantics than the observational run channel.

Current actor ordering is:

1. Apply event to published snapshot.
2. Send event to the observational channel.
3. Await registered sinks in registration order.
4. Await the run-scoped sink.
5. Poll the next core event.

Therefore:

- `TokioAgentRun::next_event` is an ordered, bounded, lossless observation.
- `AgentEventSink` is an acknowledgement barrier.
- The actor does not progress to the next event until each sink future settles.
- `RunFinished` completion and `wait_for_idle` include sink settlement.

Do not replace `AgentEventSink` with a BoltFFI callback-mode `EventSubscription`; that loses both overflow safety and acknowledgement semantics.

Rewrite the existing canonical trait into BoltFFI’s documented async-trait form:

```rust
#[async_trait::async_trait]
#[boltffi::export]
pub trait AgentEventSink: Send + Sync + 'static {
    async fn on_event(
        &self,
        event: AgentEvent,
        cancellation: CancellationToken,
    );
}
```

Swift can then implement:

```swift
final class SessionSink: AgentEventSink {
    func onEvent(
        event: AgentEvent,
        cancellation: CancellationToken
    ) async {
        await session.append(event)
    }
}
```

The actor awaiting this Swift method preserves the barrier.

Inside a sink, callers must use re-entrant capabilities such as the supplied `CancellationToken`, `cancelNow`, and `latestSnapshot`. Awaiting actor-mailbox methods from a sink can deadlock behind the sink’s own acknowledgement.

## Fix sink-only runs

`prompt_text_with_sink` currently still creates the normal bounded event receiver. `dispatch_event` sends to that receiver before invoking the sink.

If Swift uses only the sink and ignores the returned event receiver, the receiver fills and blocks the actor before later sink calls.

The existing `bindings/pi-ffi` works around this by spawning a task solely to drain `TokioAgentRun`. Do not reproduce that hidden adapter.

Either:

1. Make the observational event sender optional for sink-only runs, or
2. Require the returned run’s new `outcome(&self)` to close the unused receiver immediately.

Making the sender optional is cleaner.

## Swift consumer shapes

### Pull observation

```swift
let run = try await handle.promptText(prompt: prompt)

while let event = try await run.nextEvent() {
    switch event {
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

The final `outcome()` validates actor completion and sink barriers.

### Cancellation with lifecycle delivery

```swift
run.cancel()

while let event = try await run.nextEvent() {
    consume(event)
}

let outcome = try await run.outcome()
```

### Cancellation without further observations

```swift
let outcome = try await run.cancelAndOutcome()
```

### Acknowledged sink

```swift
let run = try await handle.promptTextWithSink(
    prompt: prompt,
    sink: SessionSink()
)

let outcome = try await run.outcome()
```

This requires a sink-only actor path or immediate closure of the unused observational receiver.

## Event envelope decision

The code defines:

```rust
AgentEventEnvelope {
    sequence,
    run_id,
    event,
}
```

and architecture Part 1 says it is the persistence/FFI form, but `TokioAgentRun` currently sends bare `AgentEvent`.

Resolve this before binding implementation. Do not let a binding layer invent an unrelated atomic sequence as the existing `pi-ffi` does.

Two legitimate choices are:

1. Preserve the current ordinary Rust API and return `AgentEvent`.
2. Promote the canonical Tokio event channel and sink contract to `AgentEventEnvelope`, making sequence/run identity authoritative inside the runtime.

For durable sessions and FFI gap detection, the second is preferable, but it should be a canonical runtime change—not a Bolt-only wrapper.

## Tokio runtime ownership

`TokioAgentHandle::new` currently requires:

```rust
tokio::runtime::Handle::try_current()
```

BoltFFI’s future polling does not supply Tokio’s reactor. The Apple package therefore needs a Rust-owned Tokio runtime retained for at least as long as:

- The actor
- Provider transports
- Tool tasks
- Environment operations

Solve this with a concrete runtime owner/factory in `pi-agent-runtime-tokio`, or another canonical runtime-owned constructor. Do not hide runtime creation in a duplicate binding facade.

## Scope the workflow around ordinary consumers

Do not require every generic provider/tool/policy/session extension seam to appear in Swift before the normal Agent API is considered valid.

For the initial Agent binding, export:

- `TokioAgentHandle`
- Reshaped `TokioAgentRun`
- Prompt/continue/retry
- Steering/follow-up/cancellation
- Reset/snapshot/wait-for-idle/shutdown
- Owned Agent event/outcome/control types
- `AgentEventSink` when acknowledged host callbacks are implemented

Keep unannotated initially:

- Bare borrowed `Agent` run streams
- `ModelRuntime` trait-object construction seams
- Scheduler streams
- Generic `TypedTool<I, F>`
- Provider implementation traits
- Local/non-Send executor family
- Raw Tokio receivers

Swift-authored tools, policies, storage backends, and provider extensions are separate callback-authoring milestones. They should not block the basic concrete Agent consumer path.

## Required acceptance tests

Add tests that prove the semantics rather than merely generated symbol presence:

1. Produce more than `DEFAULT_EVENT_CAPACITY` events with a deliberately slow Swift consumer; assert no loss or reordering.
2. Ensure `RunFinished` is always delivered.
3. End the core stream without `RunFinished`; `nextEvent()` must throw `MissingRunFinished`, not return `nil`.
4. Cancel a pending `nextEvent()`, invoke it again, and prove no event was consumed.
5. Call `outcome()` without draining more than 128 events; it must not deadlock.
6. Run sink-only delivery beyond 128 events with no hidden drainer.
7. Hold a `RunFinished` sink open; EOF, `outcome`, and `waitForIdle` must not complete early.
8. Cancel from inside a Swift sink using the supplied cancellation token.
9. Verify concurrent `nextEvent()` calls are serialized or rejected.
10. Verify no `EventSubscription<AgentEvent>` appears in the generated boundary.
11. Verify the Rust-owned Tokio runtime remains alive through actor shutdown.
12. If envelopes are selected, verify exact consecutive sequence values across multiple runs and persistence round-trips.

The design should prioritize these invariants over generated `for await` syntax. A generated async pull method is sufficient and sound; a generated `AsyncStream` backed by a drop-on-full ring is not.
