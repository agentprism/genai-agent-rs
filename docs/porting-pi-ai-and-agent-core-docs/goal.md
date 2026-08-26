# The goal

**Build the Rust crates that are pi-ai and pi-agent-core — and eventually pi-coding-agent — by porting pi's contracts and dependency boundaries, not its TypeScript implementation, into the architecture recorded in `docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part1-proposal.md` and `docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part2-revision.md`.** Part 2 takes precedence over Part 1 where they differ. The architecture was adopted by the owner on 2026-08-22 as written; it is not a starting point for negotiation.

**Why Rust:** cross-platform, FFI bindings for other languages, performance, and size. Every design choice is measured against those four, and against pi's strongest idea — `pi-agent-core` consumes `pi-ai` through narrow seams (shared types, an injected stream function, and a `Models` instance only where the harness needs auxiliary calls) and never talks to providers itself.

**The central judgment** (Part 1 §11, Part 2 closing): `Models` is the full model/provider/auth/catalog control plane; `ModelRuntime` is its narrow execution capability; `Agent` depends only on the capability; the boundary between them carries a lossless, replay-aware assistant stream — not just text and tool deltas.

## What parity means

Parity is **contract and invariant parity with pi at the pinned commit** (`8fa7eebd235355522c8104166b4f1f959b4e2f10`, `earendil-works/pi`), defined operationally — not by reproducing the bytes a JavaScript runtime happens to emit:

1. **The parity manifest** (Part 2 §10): every upstream `packages/ai/test/**/*.test.ts` and `packages/agent/test/**/*.test.ts` file maps to named Rust tests, with status `semantic-parity` or `deliberate-divergence` plus a reason. CI fails on an unmapped upstream test, a mapped Rust test that does not exist, a divergence without a reason, or a pin change without regenerating the manifest.
2. **The conformance suites** (Part 2 §10.1–§10.10) are the definition of correct behavior for streams, replay, retry, middleware, lowering, handoff, catalogs, auth, the agent loop, and the harness. Each test names its pi basis.
3. **Provider request bodies are byte-identical to pi's** for the pinned fixture corpus, for every API family in §10.8 — including the turn-two replay goldens. This is the one place where byte fidelity is the standard, and it requires the ordered, `JSON.stringify`-compatible wire writer described there.
4. **Divergences from pi are allowed only from the allowlist** (Part 2 §10.11), each with its replacement. Anything else that differs from pi is a defect.

## The commitment gates

The architecture is not considered delivered until all four pass (Part 2, "Commitment gates"):

1. **Replay gate** — all seven two-turn replay goldens pass after event assembly and a persistence round-trip.
2. **Wire gate** — default request bodies for every supported API family are byte-identical to pi for the pinned fixture corpus.
3. **Agent gate** — lifecycle, queue polling, tool scheduling, failed-message commitment, and event ordering pass the mapped pi conformance suite.
4. **Session gate** — the native durable session store passes the backend-generic storage/recovery conformance suite (serialized append, sequence validation, torn-tail recovery, atomic rewrite, operation recovery). Pi v4 byte compatibility was retired by owner ruling on 2026-08-25: this port has no existing consumers, so no backward compatibility is owed; the v4 protocol *semantics* are ported, the byte format is not.

## Implementation order

Part 1 §10's milestones, in order: contracts and `ScriptedRuntime` → agent loop against the scripted runtime → `Models` control plane → API/provider separation proven with two families → persistent credentials and FFI. Then the harness crates (Part 2 §7). No real provider is needed before Milestone 3; nothing is built against a live provider that can be built against `ScriptedRuntime`.

## Authority

pi's pinned source is the reference implementation for every behavior the manifest maps. The architecture documents are the authority for *shape*. Where an architecture document and pi source disagree about a behavior that is not on the divergence allowlist, pi is right and the document gets a correction note. The other documents in this folder are background. The previous standard — byte-observable parity with pi's JavaScript behavior, including its runtime semantics — was retired on 2026-08-22; work produced under it lives in the `ai/` crate and on `wip/parity-remediation-p01`, and may be mined for wire encoders, SSE decoders, OAuth flows, and fixtures, but it is not a baseline.

## Crate naming

The shipped crates use the `agentprism-*` family (owner decision, 2026-08-26); the architecture documents keep their original names. Map: `pi-ai` → `agentprism-ai`; `pi-agent-core` → `agentprism-core`; `pi-agent-session` → `agentprism-session`; `pi-agent-harness` → `agentprism-harness`; `pi-agent-env` → `agentprism-env`; `pi-agent-runtime-tokio` → `agentprism-runtime-tokio`; provider crates `pi-ai-<provider>` → `agentprism-<provider>`; `pi-ai-providers-all` → `agentprism-providers-all`; `bindings/pi-ffi` → `bindings/agentprism-ffi`. The bare `agentprism` name is reserved for the eventual flagship CLI. Non-renames: the `pi-messages` API family id and pi-basis test names are pi's own identifiers and keep their names; the legacy C ABI in `bindings/agentprism-ffi` keeps its `pi_`-prefixed exported symbols and `pi_ffi.h` header pending its BoltFFI replacement.
