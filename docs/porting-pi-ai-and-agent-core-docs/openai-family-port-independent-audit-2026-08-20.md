# Independent fidelity audit — openai-family port (2026-08-20)

> **ARCHIVED — historical record.** Findings A–F are RESOLVED (see §D/§E/§F). Section G was consumed as Q1 of the completed `pi-ai-full-port-v2` run and is no longer "open". This document predates [`goal.md`](goal.md), which governs: under it the Rust event carries pi's `partial` — the "partial-free protocol" listed below as "verified clean" and as "seam-#5 canon" (A12) is superseded — and any Section G row Rust can achieve is a defect, not an accepted delta. The current audit is `parity-audit-<date>.md` in this folder.

**Subject:** `agentprism-ai` crate at HEAD `2126c38` (all five phases of run `mt0ob9vi-y83ksa` committed).
**Reference:** pi-ai at pin `496185f6e4267b979e3663c45f7eb70b0c6a97b4`.
**Method:** six independent Opus auditors, launched after the run completed and independent of its per-phase reviewers. All read-only, adversarial charter (find infidelities, don't confirm; full reads of both sides; comments/test names not accepted as evidence). Scopes: P1 types+events, P2 helpers, P3 openai-completions, P4 openai-responses(+shared), P5 codex, plus one cross-cutting seam auditor over the v2 seam rulings and the crate's public surface.

**Verdict:** the hand-written porting is high-fidelity — request bodies byte-identical where we control them (`preserve_order` confirmed doing its job), event coverage/ordering, usage/cost math, retry classification, transform lowering, and the codex transport's twenty-plus audited behaviors (including the openai-beta case-sensitivity bug replicated bug-for-bug) all verified line-by-line. But the port is **not yet consumer-indistinguishable from pi-ai**. The findings split into three categories below. The largest cluster shares one root cause: **openai-oxide 0.16 does not surface HTTP response/error metadata**, and the sanctioned substrate swap required observable behavior to still match pi — it does not.

The `Models`/`Provider`/`create_provider` routing layer is not yet ported at this HEAD (later phases), so seam #2/#4/#10 runtime dispatch behavior could not be exercised; nothing below concerns that layer.

---

## A. Porting defects — pi behavior is the default; no ruling needed to fix

| # | Finding | pi | rust |
|---|---------|----|------|
| A1 | `model.api` equality gate pi does not have: completions/responses reject models whose `api` string mismatches with a terminal error; pi never gates on `model.api` (echo-only). Codex module correctly has no gate — port is internally inconsistent. | openai-completions.ts:276, openai-responses.ts:115 (echo only) | openai_completions.rs:479, openai_responses.rs:328 |
| A2 | MessageBuilder hand-rolls a partial-JSON parser instead of pi's `parseStreamingJson` semantics: omits the `repairJson` pre-pass, invents a 128-deep nesting cap, forces object-or-`{}` where pi assigns whatever the partial parse returns. Mid-stream tool-argument snapshots diverge (final args unaffected). The crate's own faithful `utils/json_parse.rs` is not what MessageBuilder uses. | json-parse.ts:32-95,104-124; proxy.ts:326 | event_stream.rs:544-597,599-829 |
| A3 | Assistant `content: null` vs omitted: assistant turn with tool calls and no text — pi puts `"content": null` on the wire; Rust omits the field entirely. | openai-completions.ts:1225,1340-1348 | openai_completions.rs:236-237,2162-2166,2283-2290 |
| A4 | Missing null-content coercion: pi coerces `content: null`/missing to `[]` inside transformMessages (legacy sessions, hand-built histories); Rust has no equivalent and fails to deserialize such messages. | transform-messages.ts:73 | transform_messages.rs:77-156; types.rs:623,777,836 |
| A5 | Tool-call-id sanitizers operate on code points; pi operates on UTF-16 code units (replace + truncate + length). Astral char → two `_` in pi, one in Rust; wire-visible id divergence. Same defect class the Go port was forced to fix (Phase 7 adjudication). | anthropic-messages.ts:1117; openai-completions.ts:1157-1158; openai-responses-shared.ts:148 | transform_messages.rs:306-318 |
| A6 | Strict decoding aborts streams pi tolerates: (a) completions typed chunk fields (`content:[]`, `reasoning:123`, float token counts → terminal error; pi duck-types and ignores); (b) responses closed enums for message content-part `type` and `phase` (unknown → stream error; pi maps unknown parts to `""` and ignores unknown phases); (c) codex/responses `service_tier` closed enum (unknown string fails terminal-event decode; pi passes through with multiplier 1). | openai-completions.ts:539-566,1491-1509; openai-responses-shared.ts:699,748; resolveCodexServiceTier | openai_completions.rs:867,901-1022; openai_responses_shared.rs:956,1085,1105-1176 |
| A7 | qwen thinking format: explicit-null `thinkingLevelMap` entry — pi's nullish `?? level` still sends `reasoning_effort: "<level>"`; Rust's defined-semantics resolver omits it. (zai correctly uses defined semantics; only qwen mismatches.) | openai-completions.ts:842 | openai_completions.rs:1720-1727,1871-1878 |
| A8 | Codex WS semantic-status failure (clean WS completion whose terminal maps to stop-reason `error`): pi appends the `provider_transport_failure` diagnostic, records the WS failure, and pins the session to SSE fallback; Rust emits the same error but performs none of those side effects. | openai-codex-responses.ts:325,348-363 | openai_codex_responses.rs:501 |
| A9 | Codex empty-body non-2xx error string: pi uses `statusText` (empty over HTTP/2 → `"Request failed"`); Rust substitutes the IANA canonical reason phrase (`"Bad Gateway"`, …). Reachable at default maxRetries=0 against chatgpt.com (HTTP/2). | openai-codex-responses.ts:428-433,1551 | openai_codex_responses.rs:1307,1580-1586 |
| A10 | Codex retryable-pattern regex: JS `.?` excludes line terminators; Rust helper matches any char, so `"rate\nlimit"` classifies retryable in Rust but not pi. | openai-codex-responses.ts:129 | openai_codex_responses.rs:1506-1515 |
| A11 | `response.failed` with empty-string incomplete reason: pi's truthy check falls to `"Unknown error (no error details in response)"`; Rust produces `"incomplete: "`. | openai-responses-shared.ts:748-753 | openai_responses_shared.rs:1859-1887 |
| A12 | Missing MessageBuilder snapshot fields pi's producers set early: `thinkingSignature` (accumulated over signature deltas), `redacted:true` + `"[Reasoning redacted]"` placeholder at thinking start, toolCall `namespace` at toolcall start. Rust events don't carry them until end events, so mid-stream snapshots lag pi's `partial` view. Fix shape (extra fields on Start/Delta events) touches the seam-#5 canon — flagged for the owner's eyes, but restoring pi-visible snapshot content is the default. | anthropic-messages.ts:620-638,691-697; openai-responses-shared.ts:485-527 | event_stream.rs:25-79,166-215 |

## B. Substrate (openai-oxide 0.16) findings — NO ruling required (corrected 2026-08-20, evidence-backed)

Initial framing called these "unfixable / needs owner ruling / needs upstream issue." That was wrong. Reading the vendored oxide 0.16 source shows public escape hatches, and our **own codex module is the in-repo precedent**: it runs HTTP+SSE over its own reqwest/tungstenite transport (`openai_codex_responses/transport.rs`, no oxide client) and exhibits none of B1-B4. Completions/responses inherit B1-B4 only because they call `OpenAI::…create_stream_raw`.

| # | Finding | Fix in our code (evidence) |
|---|---------|----------------------------|
| B1 | Duplicate `Authorization` when caller supplies a custom `authorization` header (oxide `bearer_auth` in `config.rs` + reqwest append at `client.rs:285`). | Auth is injected in `Config::build_request`; `Config` is a **public trait** (`config.rs`). A custom `Config` controls auth exactly like pi. |
| B2 | Error body loss: `extract_error` (`client.rs:808`) keeps only message/type/code/request_id; `Display` = `"API error (status N): <msg>"`. pi surfaces `"<status>: {json body}"` incl. `param`/OpenRouter `metadata.raw`. | The one item oxide's **typed** surface can't give back (`request()` is `pub(crate)`). Dissolves under the own-transport route below, which reads the raw error body directly. |
| B3 | `onResponse` fed fabricated `status:200, headers:{}`; retry-after/x-should-retry never seen. | `post_stream_json_bytes` is **public** and returns the real `reqwest::Response` (`.status()`, `.headers()`); `SseStream::new` is public. Own retry reads the headers directly. |
| B4 | Per-network-chunk `from_utf8_lossy` (`streaming.rs:58`) corrupts split multibyte chars; per-`data:`-line JSON parse. | Property of oxide's `SseStream`; our own decoder (codex buffers bytes then decodes complete frames) avoids it. |

**Resolution (no ruling, no fork, no upstream issue):** route openai-completions/openai-responses HTTP+SSE through our own reqwest path exactly as the codex module already does, keeping oxide for request/response **types** only. This eliminates B1-B4 together and lets the A-list fixes in the same functions land in one pass. The faithfulness litmus (behaviourally/architecturally/semantically faithful, idiomatic Rust) governs; nothing here needs sign-off.

## C. Language/platform-forced ε — for sign-off, with fixability noted

- Whole-number `f64` serializes `1.0` where JS emits `1` — **on the wire** for `temperature` etc.; JSON-semantically identical; fixable with a custom integral-f64 serializer if byte parity is demanded (types.rs, codex `temperature`).
- `stream()` panics out-of-band if called outside a Tokio runtime (eager `tokio::spawn`); a panic inside the task ends the stream with `MissingTerminalEvent` instead of a terminal error event. Fixable (defer spawn / catch-unwind) if desired.
- `result()` on close-without-terminal → `Err(MissingTerminalEvent)` vs pi's forever-pending promise (deliberate, test-pinned).
- `compat` paired with a custom/unfamilied `api` hard-fails `Model` deserialization; pi keeps it as inert data (runtime-tolerant, compile-time-only `never`). Interacts with the seam-#9 ruling — owner call whether to relax.
- BTreeMap/sorted key emission vs pi insertion order for env/headers/metadata maps; unknown-key drop on structs without extra-capture.
- Diagnostics `error.stack` always absent; serde vs V8 malformed-JSON detail strings; `httpdate`/`f64::parse` stricter than `Date.parse`/`Number` for exotic Retry-After forms; lone-surrogate truncation boundary; Windows `uname -r` user-agent fallback; `setTimeout(Infinity)` 1ms-vs-0ms edge.
- Architectural note: `estimate.ts` inlined privately into `simple_options.rs`; other exports of that module unavailable to future in-subset consumers.

## Verified clean (evidence base, abbreviated)

Seams #1 (no `Result::Err` escapes anywhere; setup/auth/header failures in-band), #3/#8 two-tier options, #4 header null-suppression end-to-end, #5 partial-free protocol + contentIndex addressing, #9 compat families + both-direction api invariant, #10 open unions; `ProviderImages`/`DeferredStreams`/`session_resources` grounded in pi (types.ts:289,473,479,853; session-resources.ts), not invented. P1 final wire shapes complete and round-trip faithful. P2 helpers otherwise line-faithful (hash vectors, retry math, error-body precedence, estimate/clamp, prompt-cache code-point clamp correctly *not* UTF-16). P3/P4 request construction byte-identical incl. off-spec dialect writes and null-presence semantics; all SSE event handling, usage/cost (incl. flex 0.5 / priority 2 / gpt-5.5 2.5), stop-reason mapping. P5: transport auto/fallback memory, the two single-retry WS cases, no-SSE-replay-after-WS-start, body field order incl. `store:false` / empty `prompt_cache_key`, reasoning/service-tier resolution, SSE retry loop with the non-retryable-status-retried-via-catch quirk, zstd + fallback, 5min/55min eviction, JWT extraction, openai-beta bug-for-bug, number-vs-string diagnostic codes.

---

## D. Post-repair re-audit (2026-08-20, after 6d5eb71 / f423bd7 / 25067f1)

Three fresh independent Opus re-auditors, one per repair commit, verified every A/B/C item above against pi source lines (not the audit's paraphrase) and hunted regressions in the repair diffs. **Verdict: every item RESOLVED with a regression pin; no regression away from pi in any repair diff.** Notable: R2 replaced the oxide client with `ai/src/api/openai_sse.rs` (own reqwest + byte-buffered SSE decoder; oxide retained for types only); A9 reads hyper's real wire reason phrase on HTTP/1.x and yields empty on HTTP/2.

Residue found by the re-audit, fixed in R4 (all to pi behavior, no ruling):

| # | Finding | pi | rust |
|---|---------|----|------|
| D1 | Streamed chunk with truthy top-level `error` silently dropped (completions; responses analog). pi's SDK throws APIError for any such chunk; pi surfaces `error.message` (+ OpenRouter `metadata.raw`) with stopReason=error. Rust ended with "Stream ended without finish_reason" or a spurious success when `supportsFinishReason=false`. Reachable via OpenRouter. | openai SDK core/streaming.js:49-50; openai-completions.ts:664-683 | openai_sse.rs:298-358; openai_completions.rs:781-838 |
| D2 | Known-api compat structs drop unknown keys; pi's schema-free load preserves all compat keys (the new `Custom(Value)` path already does). | types.ts:822-850 | types.rs:1273-1379,1486-1504 |
| D3 | Whole-number floats inside stringified error bodies emit `1.0` vs JSON.stringify `1`. | error-body.ts | error_body.rs:127-129; openai_sse.rs:432-455 |
| D4 | Unknown message `phase` collapsed to None and dropped from `textSignature`; pi includes any truthy phase. The test `unknown_message_content_and_phase_are_tolerated` pinned the divergent branch. | openai-responses-shared.ts:49-53,700 | openai_responses_shared.rs:1170-1179 |
| D5 | Event-only SSE message (no `data:`) silently ignored; pi's SDK dispatches `data:""` → JSON.parse throws → stream error. | openai SDK core/streaming.js | openai_sse.rs:415-418 |
| D6 | A7 (qwen explicit-null → `reasoning_effort:"<level>"`) correct in code but not pinned; a nullish→defined mutation would pass. | openai-completions.ts:842 | openai_completions.rs:1678-1685 |

Documented ε (not ported — runtime identity / language semantics, per the faithfulness litmus):
- `X-Stainless-Lang: js`, `X-Stainless-Runtime: node`, `X-Stainless-Package-Version` etc. — the OpenAI JS SDK's runtime-identity telemetry headers. Inert, and reporting a JS runtime from Rust would be false; pi's own hand-rolled codex module does not send them either.
- Off-spec usage token values that are strings/floats: pi's `x || 0` keeps a truthy string (JS concatenation in later arithmetic) or a float; Rust's `u64` usage fields coerce these to 0. Unreachable with conforming providers.
- The compat-variant type guard in completions/responses (a model whose `compat` enum variant mismatches) — a Rust type-system necessity with no pi analog; unreachable given correct api→compat routing.
- A4 normalizes `content: null` at deserialize rather than at transform; API wire identical, differs only on a raw deserialize→reserialize of a legacy session without transform.

**R4 re-audit (10b1b7a):** D1–D6 all RESOLVED with live pins; no regression in the R4 diff. Follow-up: the falsy-`error` half of D1 (SDK `if (data && data.error)`) was correct in code but unpinned — test added (`falsy_error_field_does_not_terminate_the_stream`). Remaining ε (not changed): compat reserialize hoists known keys before preserved unknown keys (serde `flatten`; values and known-key spelling identical, only interleaved-key order differs from `JSON.stringify`); a non-string truthy `phase` (number/bool) is not signed (pi's `if (phase)` would); `String(raw)` vs JSON spelling for a numeric `metadata.raw` needing exponent form. All unreachable with conforming providers.

---

## E. Delta enumeration and re-classification (2026-08-20, after 7685f3d)

Every remaining delta was re-enumerated with its pi counterpart verified by direct read. Nineteen hold as language/platform/runtime ε (documented in C/D above: `result()` vs never-settling promise; in-task panic; `BTreeMap` header/env order; `flatten` key hoisting; integer narrowing of `timeoutMs`/counts; off-spec string/float usage tokens; `error.stack`; parser detail suffix in codex JSON errors; `Retry-After` parse breadth; lone-surrogate truncation; Windows `uname` fallback; `setTimeout(Infinity)`; `X-Stainless-*`; non-string `phase`; exponent-form numeric `metadata.raw`; compat-variant type guard; `content:null` normalization point; `estimate.ts` placement; non-string JWT `chatgpt_account_id`). Two do **not** hold and were misclassified earlier — fixed in R5:

| # | Defect | pi | rust (before R5) |
|---|--------|----|------------------|
| E1 | **Abort-path fidelity.** (a) Pre-aborted signal: pi builds params, runs `onPayload`, resolves the api key, then the SDK throws at send without a wire request (`openai/client.js:357`) and `retryProviderRequest` converts any error-while-aborted into `createAbortError()` → errorMessage `"Request aborted"` (`provider-retry.ts:69-71,113-115`; bare message since status undefined, `error-body.ts:126-133`). Rust short-circuited before those steps with `"Request was aborted"`, skipped `onPayload`, preempted missing-key errors, and `send_request`'s `select!` could put a request on the wire. (b) Mid-stream abort (completions): SDK iterator exits silently (`core/streaming.js:74-75,121-122`), `finishBlock` emits `*_end` events (`openai-completions.ts:642-644`), then `"Request was aborted"` (`:645-646`); Rust returned from `next_chunk` before `finish_blocks`. (c) Codex: right messages, but abort checked before apiKey/accountId/body/`onPayload` where pi checks at the transport request points (`:257-271` vs `:383-384`, WS `:322-323`). Originally filed as "cosmetic wording" — wrong: error strings, event sequences and failure ordering are all in the litmus. | as cited | `openai_completions.rs:492-498,781-810`; `openai_responses.rs:305-311`; `openai_sse.rs` send_request select; `openai_codex_responses.rs:416-418` |
| E2 | **Routing structs drop unknown keys.** `RoutingSortOptions`, `OpenRouterMaxPrice`, `PercentileThresholds`, `OpenRouterRouting`, `VercelGatewayRouting` have no extra-capture; pi loads them schema-free and forwards `openRouterRouting` verbatim as the `provider` request field — a custom key pi sends, Rust dropped. Same class as D2, missed when D2 was scoped to compat. | `types.ts:596-599,718-800` | `types.rs:1154-1300` |

---

## F. Coverage map — the forest (2026-08-20)

The five-phase port and its repairs covered pi-ai's openai family plus substrate (~7,700 of 23,576 pi lines). Owner rulings exclude ~3,200 (lazy loading, azure, mistral, pi-messages, image generation, the agent proxy protocol). The remaining ~12,500 in-scope lines — including pi-ai's core `models.ts` (Models/Provider/createProvider/createModels/dispatch/refresh), the catalog, provider factories, `compat`, `cli`, anthropic, google, bedrock, the auth layer and OAuth flows, and a third of `utils/` — were never scheduled. Earlier "port complete" statements referred to the openai family only and overstated completeness; the compat-variant guard (E-table #17) was excused by pointing at this unported layer, which was wrong. Workflow `workflows/pi-ai-full-port.workflow.js` ports the remainder in seven gated phases (Q1–Q7), each reporting any genuinely unpreservable behavior as a four-column row for owner visibility, with the reviewer rejecting any row Rust can in fact achieve.

**R5 re-audit (93762b1):** E1(a/b/c) and E2 all RESOLVED against pi/SDK source, each pinned (incl. explicit no-wire-request and missing-key-beats-abort assertions); gates green in isolation (203 tests). Two corrections to the record: (1) R5 *did* fix codex ordering (`openai_codex_responses.rs:413-418` early abort removed) — my reading of a truncated commit stat was wrong; (2) the E1(b) scope paraphrase for responses was wrong: pi's `processResponsesStream` throws `"OpenAI Responses stream ended before a terminal response event"` (`openai-responses-shared.ts:758`) before the `signal.aborted` check at `openai-responses.ts:168` is reachable; the implementer followed pi over the scope text, correctly.

---

## G. Observed deltas still open (neutral record — pi decides the resolution)

Each row is an observed difference between this crate and pi at the pin, in already-ported code, as of `93762b1`. No classification is attached: the implementer restores pi's behavior or reports the row in the owner's four-column table, and the reviewer judges each such row on its own.

| # | Observed delta | pi counterpart |
|---|----------------|----------------|
| G1 | `result()` on a stream that closes without a terminal event resolves to an error; pi's `result()` promise never settles. | `utils/event-stream.ts:16-17,64` |
| G2 | A panic inside a provider's spawned task ends the stream with `MissingTerminalEvent`; pi's `catch` turns any thrown error into an in-band error event. | `api/openai-completions.ts:663-683` |
| G3 | `ProviderHeaders` / `ProviderEnv` serialize and iterate in sorted key order; pi keeps insertion order. | `types.ts` (`ProviderHeaders`, `ProviderEnv`), `utils/headers.ts` |
| G4 | Re-serializing compat/routing objects with preserved unknown keys emits known keys first (serde `flatten`); pi keeps original interleaving. | `types.ts:822-850`, `types.ts:718-800` |
| G5 | `timeoutMs`, `maxRetries`, `maxRetryDelayMs`, thinking budgets and usage counts are unsigned integers and reject negative/fractional JSON numbers; pi's fields are `number`. | `types.ts:163,168,176`; `Usage` |
| G6 | Off-spec usage token values (strings/floats) coerce to 0; pi's `x \|\| 0` keeps a truthy string or float. | `api/openai-completions.ts:1491-1494` |
| G7 | `diagnostics[].error.stack` is always absent; pi populates `error.stack`. | `utils/diagnostics.ts:27` |
| G8 | Malformed-JSON detail text after `"Invalid Codex SSE JSON: "` / `"…WebSocket JSON: "` is serde's message; pi's is V8's `SyntaxError` text. | `api/openai-codex-responses.ts:803,1304` |
| G9 | `Retry-After` parsing accepts RFC 7231 dates and plain decimals only; pi's `Date.parse`/`Number` accept more forms. | `utils/provider-retry.ts:61`; `api/openai-codex-responses.ts:132-157` |
| G10 | Truncation that lands inside a surrogate pair stops one character earlier; pi's `.slice` can keep a lone high surrogate. | `utils/error-body.ts:139`; `api/openai-completions.ts:1168` |
| G11 | `retry-after-ms: "Infinity"` with the delay cap disabled sleeps 1 ms; pi's `setTimeout(Infinity)` fires immediately. | `utils/provider-retry.ts:86` |
| G12 | `X-Stainless-*` request headers are not sent; pi's SDK sends them. | OpenAI SDK `client.js:612-627` |
| G13 | A truthy non-string `phase` is not carried into `textSignature`; pi `JSON.stringify`s whatever value is present. | `api/openai-responses-shared.ts:49-53,700` |
| G14 | A numeric `error.metadata.raw` is appended with serde's number formatting; pi appends `String(raw)`. | `api/openai-completions.ts:679-680` |
| G15 | completions/responses reject a `Model` whose compat enum variant is not the module's family; pi reads `model.compat` structurally and never rejects. | `api/openai-completions.ts:276` and compat getters |
| G16 | `content: null` is normalized to `[]` at deserialization, so deserialize→reserialize changes the bytes; pi normalizes only inside `transformMessages`. | `api/transform-messages.ts:73` |
| G17 | `utils/estimate.ts` is inlined privately into `simple_options.rs`; pi exports it as a module. | `utils/estimate.ts` |
| G18 | Codex JWT `chatgpt_account_id` must be a non-empty string; pi accepts any truthy value. | `api/openai-codex-responses.ts:1585-1586` |
| G19 | User-Agent falls back to `"unknown"` for the OS release where `uname -r` is unavailable; pi uses `os.release()`. (Windows is excluded by owner ruling.) | `utils/pi-user-agent.ts:18` |
