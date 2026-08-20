# Independent fidelity audit — openai-family port (2026-08-20)

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
