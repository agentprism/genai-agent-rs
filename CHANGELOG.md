# Changelog

All notable changes to this crate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Core-runtime batch (independent request fields, shared thinking-budget resolution, fallible tool
channels) plus the execution-seam batch (request-level exec-hook overrides honored by
`GenaiStreamFn`, a saturating retry-delay conversion, and the DIST-01 interim distribution
gate). The all-feature repository suite is **220/220** green.

### Added

- **Request-level exec hooks on `GenaiStreamFn`.** Per-request `StreamRequest::on_payload` /
  `on_response` hooks are now honored by the production stream function through the genai fork's
  new request-level `ExecOptions` (`exec_chat_stream_with_exec_options`): a request hook
  **replaces** the construction-time hook of its channel for that execution only — the two never
  compose, exactly one hook fires per channel per physical attempt (including retries and
  HTTP-error responses), and an absent request hook inherits the construction default installed
  via `GenaiStreamFn::with_exec_hooks`. Combined with the facade's run-admission snapshots, an
  idle `Agent::set_on_payload` / `set_on_response` replacement takes effect on the next run while
  an in-flight run keeps its snapshot.
- **DIST-01 interim distribution workflow.** `scripts/check-distribution.sh` is the release gate:
  it verifies the exact commit/version pins, the `publish = false` flag, documentation honesty
  (no registry-only install claims), and the packaged archive contents, then extracts both
  `cargo package` archives into a fresh temporary consumer, patches `genai` to the exact local
  archive equivalent of the pinned fork commit, and builds/tests the consumer without sibling
  source paths. `tests/fixtures/fresh-consumer/` is the runnable reference consumer.
- **CI gate.** `.github/workflows/distribution.yml` runs the distribution gate; no workflow
  performs any publication action.

### Changed

- **Publication is explicitly disabled.** The manifest sets `publish = false` and drops the
  docs.rs `documentation` link while the fork-only `genai` APIs remain unpublished; the `genai`
  dependency now pins the exact fork version `=0.7.0-beta.19.1-agentprism` (dual-source path
  form). The README installation section documents only the interim archive+patch flow.

### Fixed

- **Out-of-range server retry delays saturate instead of panicking.** A `retry-after` /
  `retry-after-ms` value beyond `Duration`'s range (cap disabled) now saturates at
  `Duration::MAX` in `GenaiStreamFn`'s retry layer — preserving the never-throw `StreamFn`
  contract — and cancellation still wins over the saturated sleep.

### Added (core-runtime batch)

- **Independent request fields.** `AgentLoopConfig` and `StreamRequest` gain `session_id`,
  `max_retries`, and `max_retry_delay_ms` with `with_*` builders. `AgentConfig` snapshots them into
  each run's loop configuration and the loop forwards them onto every stream request.
  `GenaiStreamFn` honors per-request `max_retries`/`max_retry_delay_ms` as overrides of its
  construction-time `RetryPolicy` (a per-request `Some(0)` disables retries for that request).
- **Fallible tool channels.** `AgentTool::try_prepare_arguments` is a new default trait method that
  adapts existing `prepare_arguments` implementations; `FnTool::with_try_prepare_arguments` is the
  closure-backed builder. `TryBeforeToolCallHook` / `TryAfterToolCallHook` come with matching
  `AgentConfig` and `AgentLoopConfig` fields, builders, and `Busy`-guarded `Agent::set_try_*`
  setters. An `Err` becomes an ordinary in-band error tool result carrying the `ToolHookError`
  display text verbatim (pi's `error.message` semantics): preparation and before-hook failures skip
  execution, and an after-hook failure replaces the completed result (content, details, usage, and
  any termination request are discarded) without rolling back tool side effects. A fallible channel
  takes precedence over its legacy counterpart when both are installed; the two are never both
  invoked for one call.
- **`ToolHookError`**, the error type of the fallible tool channels.
- **`resolve_reasoning_effort`** is public, and `AgentLoopConfig` carries an optional
  `thinking_budgets` map: prepare-next-turn thinking updates resolve through it exactly like the
  stateful agent's initial snapshot (custom budgets, `xhigh`/`max` clamping, explicit-budget
  bypass, and named-level fallback all behave identically on both paths).

### Changed

- **`session_id` is now independent of `ChatOptions::prompt_cache_key`.** Setting or clearing
  `AgentConfig::session_id` / `Agent::set_session_id` no longer writes `prompt_cache_key`, and an
  explicitly configured cache key is no longer adopted as the session id at construction or by
  `set_chat_options`. The session id is forwarded onto each stream request as
  `StreamRequest::session_id` instead. Migration: code that relied on the previous mirroring should
  set `ChatOptions::prompt_cache_key` explicitly.
- **`AgentConfig::max_retries` / `max_retry_delay_ms` are no longer passive convenience stores.**
  They are forwarded onto every run's stream requests, and a `GenaiStreamFn` stream function honors
  them as per-request overrides of its construction-time `RetryPolicy`. Applications that
  previously copied the values onto `GenaiStreamFn::with_retry` can now rely on the forwarding; the
  construction-time policy remains the default when a request carries `None`.

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
