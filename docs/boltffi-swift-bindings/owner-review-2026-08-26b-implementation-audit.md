# Owner implementation audit — ADOPTED (owner decision, 2026-08-26)

Second authoritative review. The owner adopted this audit of the revised design (branch at
674220f, audited against main at 4cef67f). Verdict: the lossless-pull direction is approved
and retained; the plan is rejected as implementation-ready until the findings below are
addressed. The design must address every finding; where a finding asserts BoltFFI behavior,
re-verify against the documentation snapshot and cite; where it asserts repository behavior,
verify at current file:line. Scope decision recorded by the session: this milestone is
option 1 — the lower-level Agent Swift SDK (TokioAgentHandle boundary); the production
coding-agent Swift SDK over the harness is a separate future milestone.

---

 Audit verdict

 I audited origin/boltffi-design at 674220f against latest origin/main at 4cef67f.

 The revised streaming direction is substantially better and should be retained. However, the plan is not implementation-ready. It still has several blocking races, packaging omissions,
 stale source assumptions, and construction-boundary problems.

 No files were changed.

 What is now correct

 The revision correctly establishes that:

 - Authoritative AgentEvent and AssistantEvent delivery must not use BoltFFI EventSubscription.
 - The Swift boundary should use async pull methods.
 - TokioAgentRun needs interior synchronization and &self methods.
 - Completion must be reusable and cancellation-safe.
 - outcome() must close abandoned observations to prevent channel deadlock.
 - Pull observation and acknowledged AgentEventSink callbacks are distinct contracts.
 - Sink-only delivery must not create an unused bounded observation queue.
 - Agent event envelopes should be allocated once before fan-out.
 - Tokio must be owned on the Rust side.

 Those are the right foundations.

 Blocking findings

 1. The branch is already stale relative to main

 The design says the pi-* to agentprism-* rename is still planned. It has already happened on main:

   pi-ai                    → agentprism-ai
   pi-agent-core            → agentprism-core
   pi-agent-runtime-tokio   → agentprism-runtime-tokio
   pi-agent-env             → agentprism-env
   bindings/pi-ffi          → bindings/agentprism-ffi

 Main also completed M8 and added agentprism-harness.

 Consequences:

 - Nearly every code path and line citation in the design is stale.
 - The workflow’s inventory phase still searches nonexistent crates/pi-* paths.
 - The workflow’s common preamble still contains the retired R2.
 - Running the workflow from scratch against current main will produce contradictory instructions or fail inventory.

 The branch should be rebased onto origin/main, then the workflow, inventory, owner-review references, and design must use the current crate names.

 The workflow should replace the retired R2 in COMMON; do not leave it active and supersede it only inside the Design phase.

 2. accept_run has an uncovered cancellation race

 Current accept_run effectively does:

   if accepted.is_closed() {
       return false;
   }

   idle_tx.send(false);
   accepted.send(Ok(())).is_ok()

 Cancellation can happen between is_closed() and send():

 1. The caller drops accepted_rx.
 2. The actor publishes idle = false.
 3. accepted.send(Ok(())) fails.
 4. The actor drops the unaccepted stream.
 5. Nothing restores idle = true.

 The proposed RunEstablishmentGuard does not repair this because this race occurs before successful acceptance.

 Fix accept_run so failed acceptance restores idle:

   let _ = idle_tx.send(false);

   if accepted.send(Ok(())).is_err() {
       let _ = idle_tx.send(true);
       return false;
   }

   true

 Add a deterministic test for the check-to-send race. Test 19 currently covers only cancellation after successful actor acceptance.

 3. The establishment guard does not cover the complete foreign handoff race

 The plan protects:

   actor accepted
       ↓
   Rust establishment future still pending
       ↓
   RunEstablishmentGuard::handoff

 But another race remains:

   Rust future returns Ready(TokioAgentRun)
       ↓
   Swift task is cancelled before retaining the generated class
       ↓
   BoltFFI cleanup drops the unclaimed Rust output

 Once the guard is disarmed, dropping that TokioAgentRun currently only drops its receiver. It does not necessarily cancel the accepted run.

 The design needs an explicit established-run drop policy:

 - Dropping an active TokioAgentRun should close observation and cancel its run token, or
 - A separate actor-owned run lease must cancel when no consumer/sink owns the run, or
 - BoltFFI’s exact completed-class-result handoff must be proven to transfer/release this resource safely.

 Add a test after the Rust future becomes ready but before Swift retains the class. The existing pre-handoff test is insufficient.

 The same issue applies to TokioAssistantStream.

 4. TokioAssistantStream can retain the runtime indefinitely when dropped

 The proposed assistant producer owns a RuntimeLease. If the established TokioAssistantStream is dropped while the provider is pending and produces no further events:

 - The receiver disappears.
 - The cancellation token is not necessarily cancelled.
 - The producer may never attempt another channel send.
 - It may never notice receiver closure.
 - Its runtime lease may never be released.

 cancelAndWait() cannot be the only safe cleanup mechanism because foreign objects can be released without explicitly calling it.

 Drop or an equivalent ownership lease must cancel established assistant work. Add a provider-pending-forever test that drops the class without calling cancelAndWait() and proves runtime
 teardown.

 5. Sink-only semantics silently change an existing API

 Current:

   prompt_text_with_sink(...)
       -> TokioAgentRun

 provides both:

 - A run-scoped acknowledged sink.
 - A normal pull-observable TokioAgentRun.

 The revised plan changes that same method into sink-only delivery with no observation sender. That is a semantic breaking change hidden behind the existing name.

 Use one of:

   prompt_text_with_sink(...)          // preserves pull + sink
   prompt_text_sink_only(...)          // no observational sender

 or an explicit canonical observation mode.

 Do not silently change the established method from “sink plus run events” to “sink only.”

 6. EOF rules contradict sink-only and abandoned-observation runs

 Section 4.2 requires normal EOF only after RunFinished was delivered to the consumer.

 But:

 - Sink-only runs intentionally deliver no pull events.
 - outcome() intentionally discards buffered events.
 - next_event() after outcome() is specified to return Ok(None).

 These cannot all satisfy “consumer received RunFinished.”

 The state model must distinguish:

 Pull observation remained active: Ok(None) requires RunFinished was delivered through this pull cursor; completion matches its outcome; sinks settled.

 Observation was deliberately closed or never installed: Ok(None) requires the actor internally reached and validated RunFinished; completion and sinks settled; consumer delivery is not required because observation was explicitly abandoned.

 Track producer terminal validation separately from consumer terminal delivery.

 7. Concurrent pull serialization does not guarantee consumer ordering

 The design chooses to serialize concurrent nextEvent() calls with a mutex and then requires two Swift tasks’ merged results to remain in source order.

 The mutex guarantees: each event is received once; receiver access is serialized.

 It does not guarantee: which task acquires the mutex first; which task processes/publishes its returned value first; that merging task results reconstructs source order without sorting by envelope sequence.

 TokioAgentRun is logically a single-consumer cursor. Prefer rejecting concurrent pulls:

   TokioAgentError::ConcurrentEventPoll

 This avoids accidentally legitimizing an event stream split across tasks.

 If serialization is retained, test uniqueness and sequence completeness, not callback-processing order across two tasks.

 8. Envelope sequencing has an unaddressed invariant-error gap

 The design allocates envelope sequence from pre-apply snapshot.next_sequence, then dispatches only after apply_event_to_snapshot succeeds.

 But apply_event_to_snapshot increments sequence before all validation is complete. If it returns SnapshotInvariant:

 - The underlying Agent/core sequence has already advanced.
 - No envelope is delivered for that sequence.
 - A later run can begin with a visible sequence gap.

 Since the design promotes envelopes as the authoritative persistence/FFI sequence, it must define this path.

 Possible resolutions: allocate authoritative envelopes in core before the actor mirror; deliver the rejected event envelope before reporting actor failure; add a durable actor-failure envelope/event; roll back only if the core sequence can also be rolled back safely.

 Add a test: force SnapshotInvariant, start another run, and verify the selected global sequence policy.

 Packaging and architecture blockers

 9. There is no defined BoltFFI root library

 BoltFFI packaging needs one source library/static library whose dependency graph reaches all annotated crates.

 The plan proposes annotations across agentprism-ai, agentprism-core, agentprism-runtime-tokio, and provider crates, but never selects the root crate linking them into one artifact.

 This cannot be deferred as “multi-crate discovery unresolved.” Even if dependency scanning works, the root dependency direction must exist.

 The design must identify: the canonical root crate; its dependency graph; which crate owns staticlib; which crate owns build.rs; which crate is selected in boltffi.toml; how provider and runtime symbols are both reachable without introducing dependency cycles.

 Do not add BoltFFI build scripts and staticlib configuration indiscriminately to every canonical crate.

 10. The required xtask package-apple pipeline is missing

 The agreed packaging design was repository-owned:

   xtask package-apple → pin BoltFFI → generate and validate Swift → build Apple slices → create XCFramework → emit SwiftPM metadata

 The revised plan instead falls back to BoltFFI’s generic packaging flow and never specifies xtask package-apple.

 Add an explicit packaging phase covering: exact BoltFFI version pin; generator/source compatibility check; generated API completeness check; macOS/iOS/device/simulator slices; XCFramework creation; SwiftPM package generation; Swift XCTest execution; artifact/module naming; reproducibility from a clean checkout.

 There is currently no BoltFFI version pin in the design. The reviewed source behavior was BoltFFI 0.30.1; that or another verified version must be pinned exactly.

 11. Swift class Sendable is unaddressed

 Current BoltFFI’s Swift class template generates:

   public final class TokioAgentRun { ... }

 not a Sendable conformance.

 The design’s Swift tests and examples pass Rust-backed classes among multiple Tasks. Under Swift 6 strict concurrency, this may fail compilation even though the underlying Rust object is Send + Sync.

 The plan must resolve generated class concurrency explicitly: upstream/fix BoltFFI class generation; support generated @unchecked Sendable only for Rust classes proven Send + Sync; or avoid cross-task class transfer.

 A required handwritten Swift extension would violate the no-required-wrapper goal. The resolution must be generator-owned and tested under Swift 6 strict concurrency.

 12. There is no generated-surface completeness gate

 The plan checks selected methods and forbids EventSubscription, but it does not enforce:

 │ Every public method added to an annotated impl appears in generated Swift.

 Add a generation manifest or source-to-generated contract check: enumerate every annotated type/impl/method; compare against generated contract/Swift symbols; fail if an annotated item disappears; fail if a new public method in an annotated impl is silently skipped; require an explicit #[skip] plus reason for deliberate exclusions.

 This was one of the primary reasons to choose BoltFFI’s whole-impl projection.

 13. Implementation order is too risky

 Phase 1 currently proposes all of the following before proving essential BoltFFI behavior: actor/run redesign; runtime supervisor and leases; assistant producer; envelope promotion; async callback rewrite; native HTTP transport; OpenAI factory and credentials; nineteen Rust test families.

 Only in phase 2 does it discover whether: multi-crate scanning works; cfg_attr works; tuple payloads generate; nested errors generate; owned class callback arguments generate; non-exhaustive enums generate; the complete value graph generates.

 Add a Phase 0 disposable/probe target that pins BoltFFI and proves these mechanics before broad canonical changes. It can use canonical leaf types and test-only fixtures without becoming a production facade.

 Then implement in smaller slices:

 1. Bolt capability/root-package/Swift-6 probe.
 2. TokioAgentRun pull/outcome/cancellation.
 3. Envelope promotion and sink semantics.
 4. Rust-owned runtime.
 5. Direct model stream.
 6. Production provider/auth construction.
 7. Complete value graph.
 8. Apple packaging.

 Production construction concerns

 14. OpenAiModelsFactory puts transport in the wrong layer

 The plan proposes NativeOpenAiHttpTransport inside the OpenAI provider crate.

 HttpTransport is a provider-neutral injected seam used across all provider families. A native reqwest implementation should be provider-neutral, for example agentprism-transport-reqwest or another Tokio/native transport crate.

 Putting it inside agentprism-openai: couples a provider leaf to one native executor/transport; duplicates transport work when adding Anthropic, Google, Codex, etc.; undermines the existing ProviderInputs { http, environment } design; makes the first generated API artificially OpenAI-specific.

 Prefer: ReqwestTransport → agentprism-providers-all / concrete provider factories → Models. Provider leaves should continue consuming Arc<dyn HttpTransport> internally.

 15. The factory bypasses the intended auth/control-plane flow

 The proposed OpenAI factory accepts an API key string, seeds an in-memory credential store, builds only OpenAI, and does not exercise persistent credentials or interactive login.

 That is not representative of the target consumer flow previously selected: Models owns provider/auth/catalog control; credentials can persist; Swift can call check_auth/login; OpenAI Codex uses device-code OAuth; the target model is gpt-5.6-sol with xhigh reasoning.

 At minimum the design should not make API-key OpenAI the sole “production” construction path. Prefer a provider-neutral native Models factory that: installs the concrete native transport; registers selected or all built-in providers; accepts a canonical credential store; preserves Models::login and auth interactions; supports OpenAI Codex without a new transport implementation.

 Add a captured/no-live-network acceptance path for: openai-codex / gpt-5.6-sol / ReasoningLevel::Xhigh, including authentication or a fixture-backed persisted credential.

 16. TokioRuntimeOwner conflates executor and application assembly

 The proposed owner stores Models, ToolRegistry, runtime supervisor, and channel capacities, and also spawns agents and starts direct model streams.

 That is more than runtime ownership. It is also an Agent factory and model execution facade.

 The architecture calls for explicit ModelRuntime injection and an AgentFactory, while Models remains the independent full control plane.

 Prefer separating: TokioRuntimeOwner owns executor/supervisor only; AgentFactory or TokioAgentFactory owns/clones Models + ToolRegistry and uses TokioRuntimeOwner to spawn actors; Models remains auth/catalog/provider control plane.

 If the combined type is retained, rename and document it as a factory rather than claiming it is only a runtime owner.

 Scope clarification

 Main now includes the completed agentprism-harness.

 If this package targets the lower-level Agent SDK, exporting TokioAgentHandle is valid. If it is intended as the eventual coding-agent product boundary, the design is incomplete because it bypasses: durable sessions; harness orchestration; environment capabilities; compaction; skills and prompt templates; harness events.

 The design should state explicitly whether this milestone is:

 1. The lower-level Agent Swift SDK, or
 2. The production coding-agent Swift SDK.

 Do not call the bare actor path the complete production harness unless option 1 is explicitly intended.

 Final assessment

 Approve the revised lossless-pull direction. Reject the current plan as implementation-ready.

 The next revision should first address:

 1. Rebase/rename/workflow correction.
 2. accept_run cancellation and established-output drop races.
 3. EOF/sink-only contract consistency.
 4. Root crate plus xtask package-apple.
 5. Swift Sendable.
 6. Provider-neutral transport and proper Models auth construction.
 7. Phase-0 BoltFFI capability probe.
 8. Generated API completeness enforcement.

 After those corrections, the plan will be a credible implementation blueprint rather than only a thorough design inventory.
