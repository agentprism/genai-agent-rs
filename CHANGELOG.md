# Changelog

All notable changes to this crate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-08-07

Production-parity release bundling parity batches 1–3 and a release wrap-up. The pinned
non-harness parity matrix holds at **55/55** mapped cases (repinned in batch 1); the all-feature
repository suite is **149/149** green. No mapped divergence from the TypeScript reference was
introduced.

### Added

- **Cost and usage accounting (batch 3).**
  - `AgentUsage::cost: Option<AgentCost>` plus the `AgentCost` component-cost struct
    (`input`/`output`/`cache_read`/`cache_write`/`total`).
  - `AgentUsage::cache_write_1h_tokens` and `AgentUsage::reasoning_tokens`, mapped from
    `genai::chat::Usage` details (`cache_creation_details.ephemeral_1h_tokens` and
    `completion_tokens_details.reasoning_tokens`). genai zero-elides `reasoning_tokens`, so
    `Some(0)` is unreachable.
  - Injectable `PriceCatalog` trait plus `compute_cost` and the `ModelCost` / `ModelCostTier` /
    `ModelCostRates` types (`pricing.rs`), porting pi-ai's model-catalog cost step: the highest
    tier whose `input_tokens_above` is strictly below `input + cache_read + cache_write` prices the
    whole request, and the 1h cache-write split is priced at an explicit `cache_write_1h` rate
    (falling back to `cache_write`).
  - `GenaiStreamFn::with_price_catalog` and the reusable `attach_cost` finalization hook, which set
    cost on the terminal message's usage at stream finalization; `AgentConfig::price_catalog` as an
    application-side convenience store.

- **Reasoning budgets and transport advisory (batch 2).**
  - `ThinkingBudgets { minimal, low, medium, high }` on `AgentConfig`, resolving a named
    `ThinkingLevel` to `ReasoningEffort::Budget` with `xhigh`/`max` clamping through `high`. There
    is no implicit default budget table, and an explicit `ThinkingLevel::Budget(n)` bypasses the
    map. `Agent::set_thinking_budgets` is `Busy`-guarded.
  - `Transport { Sse, Websocket, WebsocketCached, Auto }` accepted on `AgentConfig` and
    `StreamRequest` and forwarded onto every request. `Agent::transport` / `Agent::set_transport`
    are deliberately unguarded (matching the TS CLI's live reassignment). The SSE-only
    `GenaiStreamFn` ignores the advisory, which the TS contract permits.

- **Release-mechanics wrap-up.** `#[non_exhaustive]` plus complete `with_*` builder coverage on
  `AgentConfig`, `AgentLoopConfig`, `StreamRequest`, `AgentUsage`, `AgentCost`, `ThinkingBudgets`,
  `Transport`, `AgentError`, and `LoopError`, so later parity work that adds fields or variants
  stays semver-minor. `AgentState` is intentionally left exhaustive so applications can keep
  constructing it with functional-update syntax (`..AgentState::default()`).

### Changed

- **Behavioral parity with the TypeScript reference (batch 1).**
  - An empty-string tool block reason now falls back to `"Tool execution was blocked"`, mirroring
    the TS `||` falsiness.
  - An empty-string assistant `error_message` no longer populates `AgentState::error_message`,
    matching TS truthiness.
  - Site-specific `AgentError::Busy(BusyContext)` message texts (prompt / continue / reset) and the
    `NoDefaultStreamFn` text are pinned byte-for-byte to the TS strings.
- `AgentUsage` no longer derives `Eq` (its optional `cost` carries floating-point dollar amounts);
  `PartialEq` is retained.
- **Documentation.** Corrected the proxy wire-protocol claims — the V1 request schema is
  crate-defined and not compatible with the TypeScript `proxy.ts` request contract — and refreshed
  the README compatibility table and `docs/parity-roadmap.md`.

### Notes

- Optional serde wire renames for TS-JSON interop were deliberately not taken; JSON interop with the
  TypeScript types is not a goal for this release.
- This crate builds against upstream `genai` main semantics. Upstream-gated parity items
  (streaming-error response headers, `ToolResponse` binary parts, exec payload/response
  interceptors) remain tracked in `docs/parity-roadmap.md` §2.

## [0.1.0]

Initial release. Provider-neutral streaming agent loops and a cloneable stateful `Agent` facade,
ported from the non-harness contract of `@earendil-works/pi-agent-core`: the `GenaiStreamFn`
provider boundary, schema-validated tools, ordered lifecycle events, steering and follow-up queues,
cooperative cancellation, the optional `testing` (mock/scripted streams) and `proxy` (authenticated
HTTP/SSE) features, and the M1–M6 parity matrix.

[0.2.0]: https://docs.rs/rust-genai-agent/0.2.0
[0.1.0]: https://docs.rs/rust-genai-agent/0.1.0
