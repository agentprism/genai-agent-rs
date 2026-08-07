# Parity roadmap

Production-scoped follow-up work from the faithfulness verification of this crate against
`@earendil-works/pi-agent-core` (pi commit `6b461b75b39b5a19b378dc42fbfbd1655bc446a6`).

Scope rule: an item qualifies only if the behavior **ships in production** through the pi CLI
(`pi/packages/coding-agent`), the sole production consumer of the package's non-harness surface.
Features that are merely exported (declared-only) are listed under "Dropped" with evidence.

Verification summary: the ported core is faithful — all 52 upstream non-harness test cases are
mapped and green, and every production-critical behavior (awaited-listener run settlement,
`prepareNextTurnWithContext` refresh, queue drain modes + `continue()` semantics, the tool
pipeline including `prepareArguments`/per-tool sequential/blocked-`terminate`/`addedToolNames`/
`tool_execution_update`, `message_update` partials, copy-on-assign state, the `length`-stop
failure path) is already correct. The items below are the remaining gaps.

## 1. Do now — production-shipped, no upstream `genai` changes

All section-1 items have shipped as of the 0.2.0 release (parity batches 1–3 plus the release
wrap-up). The **Status** column records where each landed.

| Item | Status | Production evidence (pi) | Design | Effort |
|---|---|---|---|---|
| `AgentUsage.cost` + injectable `PriceCatalog` trait | ✅ Done (batch 3) | `$` footer (`footer.ts:142-144`), `/usage` breakdown (`usage-totals.ts`), cache economics (`cache-stats.ts:78-81`); pi-ai computes cost from the model catalog (`models.ts:878-898`) | **Landed (batch 3).** `cost: Option<AgentCost>` on `AgentUsage`; `PriceCatalog` trait + `compute_cost` (`pricing.rs`) with the pi tier rule (highest tier whose `input_tokens_above` is strictly below `input + cache_read + cache_write` prices the whole request). Attached at stream finalization via `GenaiStreamFn::with_price_catalog`; `AgentConfig::price_catalog` is an app convenience store (the facade builds no stream fns). `None` unless a catalog is configured and prices the model. Catalog data stays app-supplied. | M |
| `AgentUsage.cache_write_1h_tokens` + `reasoning_tokens` | ✅ Done (batch 3) | Summed in production compaction (`compaction.ts:105-109`); `cacheWrite1h` drives Anthropic 2× 1h-cache pricing (`models.ts:890-895`) | **Landed (batch 3).** Two optional fields mapped from `genai::chat::Usage` details (`assistant.rs`): `cache_write_1h_tokens` from `prompt_tokens_details.cache_creation_details.ephemeral_1h_tokens`, `reasoning_tokens` from `completion_tokens_details.reasoning_tokens`. genai zero-elides `reasoning_tokens`, so `Some(0)` is unreachable — documented. `compute_cost` prices the 1h split at an explicit `cache_write_1h` rate (fallback to `cache_write`), generalizing pi-ai's hardcoded 2× rule. | S |
| `thinking_budgets` per-level map | ✅ Done (batch 2) | Passed at the production `new Agent(...)` site (`sdk.ts:358`) from settings | **Landed (batch 2).** `ThinkingBudgets {minimal, low, medium, high}` on `AgentConfig` (`config.rs`); resolve named level → `ReasoningEffort::Budget` with pi-ai clamping (`xhigh`/`max` → `high`); explicit `ThinkingLevel::Budget(n)` bypasses the map; no implicit default table. maxTokens-fitting is impossible without a model catalog — documented on `ThinkingBudgets`. `set_thinking_budgets` is `Busy`-guarded. | S |
| `transport` advisory option | ✅ Done (batch 2) | Passed at construction (`sdk.ts:357`) and live-reassigned from settings UI (`interactive-mode.ts:4454`) | **Landed (batch 2).** `Transport {Sse, Websocket, WebsocketCached, Auto}` (`config.rs`) accepted on `AgentConfig`/`StreamRequest` and forwarded by the loop; `GenaiStreamFn` ignores it (TS contract: unsupporting providers ignore it — compliant). `set_transport` is unguarded, matching how the CLI reassigns it. | S |
| Behavioral parity batch | ✅ Done (batch 1) | Blocked-tool path is production-used via extension `tool_call` hook (`agent-session.ts:480-499`) | (a) empty-string block reason falls back to `"Tool execution was blocked"` (`tool_exec.rs:392-397`, mirror TS `\|\|` falsiness); (b) empty-string `error_message` must not populate state (`agent.rs:1237-1241`); (c) three site-specific busy texts via `AgentError::Busy(BusyContext)`; (d) `NoDefaultStreamFn` message text parity. | S |
| Proxy documentation correction | ✅ Done (batch 1) | — (honesty fix) | `src/proxy/mod.rs:22-23` and README claim request-wire compatibility with the TS protocol; false — the V1 request schema is crate-defined. Correct the claims. Docs only. | S |
| Release mechanics | ✅ Done (0.2.0 wrap-up) | — | Bundled batches 1–3 in the 0.2.0 release and added `#[non_exhaustive]` + complete builder coverage to `AgentConfig`, `AgentLoopConfig`, `StreamRequest`, `AgentUsage`, `AgentCost`, `ThinkingBudgets`, `Transport`, `AgentError`, and `LoopError` so later parity work stays semver-minor. `AgentState` is deliberately left exhaustive because README examples build it with functional-update syntax, which `#[non_exhaustive]` forbids outside the crate. The optional serde wire renames were **not** taken — TS-JSON interop is not a goal for this release. | S |

## 2. Upstream-gated — production-shipped, requires `rust-genai` changes (PR in flight)

Target repo: `jeremychone/rust-genai`, via the `agentprism` fork. One PR, one commit per block.

| Block | Production evidence (pi) | Upstream change | Follow-up here |
|---|---|---|---|
| Headers on streaming HTTP errors | `maxRetryDelayMs` passed at construction (`sdk.ts:359`); pi-ai retry policy reads `retry-after-ms`/`retry-after`/`x-should-retry` headers and caps server-requested delays (`provider-retry.ts`) | Streaming-path HTTP failure surfaces response headers (today the streaming error carries only status/body — non-streaming `webc` keeps headers, streaming does not) | Cancel-aware retry layer inside `GenaiStreamFn` (peek first event, retry handshake only, never mid-stream), pi-exact cap message; `max_retries`/`max_retry_delay_ms` config (M) |
| `ToolResponse` binary parts | Production tool results carry images; `afterToolCall` normalizes them (`agent-session.ts:501-532`) | Optional binary attachments on `ToolResponse`, covering all adapters per the pi-ai reference behavior: native blocks where the wire supports them (Anthropic `tool_result`; Gemini 3+ `functionResponse.parts`), and a follow-up user message ("Attached image(s) from tool result:" + image parts, with a "(see attached image)" placeholder in the tool slot) on string-only wires (OpenAI-compatible, older Gemini) — `openai-completions.ts:1243-1315`, `google-shared.ts:189-210` | Converter attaches parts instead of the `"[image omitted]"` marker (`message.rs:339-353`) (S) |
| Exec interceptors (`onPayload`/`onResponse`) | Extension bridge hooks `before_provider_request`/`after_provider_response` wired at the production construction site (`sdk.ts:331-348`) | Per-request payload interceptor (may replace the provider payload before send) + response observer (status + headers, before body consumption). genai today has no interceptor and the streaming HTTP send is lazy — needs threading through the stream setup | `on_payload`/`on_response` hooks on `AgentConfig` + `StreamRequest`, invoked by `GenaiStreamFn`; proxy honors them directly (M) |

## 2b. Backend gap — production-shipped in pi, unreachable from the Rust stack today

| Item | Production evidence (pi) | Design options |
|---|---|---|
| OpenAI Codex backend (ChatGPT-plan) | pi-ai's dedicated `openai-codex-responses` provider: base URL `https://chatgpt.com/backend-api`, ChatGPT OAuth auth, WebSocket transport with SSE fallback; the CLI's settings migrate a legacy `websockets` boolean to the `transport` enum, showing real production use | Distinguish two "Codex" meanings: genai fully supports Codex **models on the standard OpenAI API** (name-routed to the OpenAI/OpenAI-Responses adapters per README:146-147, API-key auth) — those need nothing. What genai lacks is the **ChatGPT-subscription Codex backend** (different endpoint, OAuth, WebSocket). Options: (a) new genai adapter incl. a WebSocket transport genai entirely lacks (L, upstream); (b) a self-contained `CodexStreamFn` in this crate behind the `StreamFn` boundary (no upstream changes; pragmatic first step). Auth note: in pi, the login/token-cache/refresh flows live in **pi-ai** (`ai/src/auth/`: credential-store, resolve, and per-provider OAuth flows incl. `openai-codex.ts`), NOT in pi-agent-core, whose only auth surface is the `getApiKey` pass-through hook. genai declines that role (no OAuth machinery; Vertex consumes an app-supplied bearer via `AuthResolver`, `vertex/adapter_impl.rs:137`). Faithful adaptation: a sibling `rust-genai-auth` crate (the structural equivalent of pi-ai's `auth/` module, packaged beside genai) owning login/token-cache/refresh — starting with the ChatGPT Codex flow (browser + device-code, mirroring `auth/oauth/openai-codex.ts`) — feeding genai via `AuthResolver` or the `CodexStreamFn`. `rust-genai-agent` stays auth-agnostic like pi-agent-core. The novel transport work (WebSocket) remains separate from auth. |

## 3. Dropped — no production consumer (do not build without new evidence)

| Item | Why dropped |
|---|---|
| `getApiKey` per-call resolution | Declared-only: never passed or assigned in coding-agent; auth resolves in `ModelRuntime.prepareRequest`. (Design exists — needs no upstream change — if a consumer appears.) |
| Deferred responses (`stopReason: "deferred"`, `DeferredHandle`, fetch/cancel) | Unreachable in the production non-harness path: only pi-ai's synthetic test provider implements it; the non-harness loop has no deferred branch. `deferredToolsMode: "kimi"` is an unrelated `addedToolNames` compat flag. |
| `pi-proxy` wire module (`streamProxy` compatibility) | `streamProxy` has zero production callers in pi's entire history; no server anywhere implements `POST /api/stream`; its intended in-repo consumer (`web-ui`) was removed without adopting it. Only the doc correction (§1) remains warranted. |
| `constrainedSampling` grammar modes | No production tool uses grammar constraints; genai's `custom_format` already covers OpenAI grammar tools if needed later. |
| Redacted thinking, `responseModel`, `diagnostics` | No identified production consumer in coding-agent; first two also upstream-gated. |
| `shouldStopAfterTurn`, `prepareNextTurn` (no-context), global `toolExecution: "sequential"`, `reset()`, `waitForIdle()`, `prompt(string, images)` | Declared-only in production; already ported anyway — no action. |
| Mid-run hook mutability, construction-time stream-fn capture, bug-for-bug hang reproduction | Production assigns hooks between construction and first run only (compatible with the documented Busy policy); the TS behaviors Rust hardens away are hangs/unhandled rejections. Mechanisms documented in the investigation if ever mandated. |

## References

- Verification + designs: session investigation reports (agent loop, Agent facade, proxy, type
  layer, harness feasibility, production-usage audit), 2026-08-06.
- Parity manifest: `tests/parity_manifest.toml` (52/52 mapped, pinned upstream commit).
- Harness port feasibility (out of scope, design study only): see
  [`harness-port-design.md`](harness-port-design.md) — separate `rust-genai-agent-harness`
  crate, ~10–14 engineer-weeks, JSONL interop achievable.
