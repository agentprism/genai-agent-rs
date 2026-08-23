# Pi provider request fixture corpus

This is the M4.1 capture corpus required by Architecture v2 part 2 §10.8. It was generated from Pi commit `c49906ec77788625aacbdc53ebca6fbe65bd20f5` on 2026-08-23.

The checked `openai-completions/` and `anthropic-messages/` trees are deterministic captures from Pi's real API-family modules. A local Bun server records the exact request bytes and returns fixed SSE frames. Pi assembles turn one, the tool appends a fixed tool result or user message, and **the same logical entrypoint (`stream` or `streamSimple`) is invoked for turn two**. Capture-time assertions reject a turn-two request that loses simple-option reasoning/thinking, sampling overlays, or max-output clamping.

The additional `credential-backed/` tree was captured through the same local server acting as a reverse proxy to live endpoints. It proves that the tool can capture real provider frames with available credentials while keeping credentials out of the artifacts and tests.

The M4.1 Rust tests validate corpus provenance, completeness, redaction, digests, and canonical `JSON.stringify` form. They deliberately do **not** use the §10.8 exact wire/replay conformance names: those remain planned until the OpenAI Completions and Anthropic Messages Rust encoders can consume the canonical fixtures, and replay fixtures can pass through `AssistantAssembler`, persistence, continuation append, and turn-two Rust encoding before byte comparison.

## Artifact layout

Each `<family>/<case>/` and `credential-backed/<family>/<case>/` directory contains:

- `canonical.json`: canonical model/context/options, logical entrypoint, turn-two append, and deterministic request inputs;
- `request-turn-1.body.json`: exact compact request bytes emitted by Pi, with no trailing newline;
- `request-turn-1.headers.json`: stable semantic headers with authentication redacted;
- `response-turn-1.sse`: exact frames consumed by Pi;
- `request-turn-2.body.json`: exact compact turn-two request bytes emitted by Pi;
- `request-turn-2.headers.json`: stable redacted turn-two header metadata;
- `metadata.json`: capture mode, credential source label, redaction state, and SHA-256 digests.

Header artifacts intentionally omit `host`, framing/compression headers, `user-agent`, and `x-stainless-*` SDK/runtime fingerprint headers. Their names are recorded in `omittedRuntimeHeaders`; logical provider, model, request, beta, session-affinity, and authentication headers remain, with secrets replaced by `[REDACTED]`.

Hermetic deterministic inputs are timestamp `1700000000000`, session ID `session-m4-00000000`, request ID `request-fixture-0001`, and fixed response/tool-call IDs. Credential-backed captures retain provider-generated response IDs, call IDs, and replay signatures verbatim because those bytes are the replay golden; deterministic canonical request inputs remain fixed.

## Hermetic cases captured

All §10.8 cases applicable to these two families are captured for both families:

- text-only;
- system/developer prompt;
- images;
- thinking disabled;
- reasoning `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`;
- signed thinking replay;
- redacted/encrypted reasoning replay;
- one tool call;
- multiple tool calls;
- tool results;
- tool-result images;
- orphan-result repair;
- cache disabled, short, and long;
- sampling defaults and overrides;
- max-output clamp;
- strict tool schema;
- provider/model headers;
- session affinity;
- API-specific compatibility flags;
- cross-provider handoff;
- failed-turn omission.

The minimum acceptance cases are `text-only`, `one-tool-call`, `tool-results`, and `signed-thinking-replay`. `redacted-encrypted-reasoning-replay` additionally supplies Anthropic redacted-thinking and OpenAI encrypted-reasoning turn-two bodies.

## Credential-backed capture record

At capture time, `DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, and `OPENROUTER_API_KEY` were available. `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` were unavailable. The Pi auth store contained `anthropic`, `openai`, `openai-codex`, and `github-copilot` credentials; the stored Anthropic OAuth access token was expired, so it was not refreshed because refresh-token rotation would require updating a credential file outside this task's allowed paths.

The checked credential-backed corpus captured all four acceptance cases for both families:

- `openai-completions`: `text-only`, `one-tool-call`, `tool-results`, and `signed-thinking-replay`, using `OPENROUTER_API_KEY` with `openai/gpt-oss-20b`;
- `anthropic-messages`: the same four cases, using `OPENROUTER_API_KEY` with OpenRouter's Anthropic Messages endpoint and `anthropic/claude-haiku-4.5`.

No acceptance case remained unavailable. Before selecting the OpenRouter Anthropic endpoint, the same four Anthropic cases were attempted with an in-memory refresh of the Pi auth-store `github-copilot` credential. The account's enabled Claude model was rejected by the `/v1/messages` endpoint as `model_not_supported`; no GitHub credential was written or checked in.

The remaining §10.8 cases are present in the deterministic corpus but were not repeated against live credentials. They exercise synthetic compatibility branches, fake model/request headers, unsupported reasoning-level matrices, cache modes, cross-provider history, image payloads, and failure repair; repeating them would add live cost and provider-dependent failures without changing Pi's captured request-construction bytes. `credential-backed/report.json` is the machine-readable record of the live acceptance run.

`DEEPSEEK_API_KEY` was not selected because the OpenRouter model supplied the required structured reasoning replay. `GEMINI_API_KEY` targets a different API family. The OpenAI/OpenAI-Codex auth-store entries target Responses-family APIs rather than this package's `openai-completions` capture.

## Regeneration

From the capture-tool directory:

```sh
cd providers/fixtures/capture-tool
bun install --frozen-lockfile
PI_PIN_DIR=/home/vikash/pi-pin-c49906ec7 bun run capture
```

The tool verifies the pinned commit without spawning Git, copies only `packages/ai/src` into ignored scratch space, and loads the copy against the locked dependencies. It never modifies the Pi checkout.

To repeat the credential-backed acceptance capture:

```sh
PI_PIN_DIR=/home/vikash/pi-pin-c49906ec7 bun run capture:credential
```

`PI_FIXTURE_FAMILIES` and `PI_FIXTURE_CASES` accept comma-separated selections. `PI_FIXTURE_ANTHROPIC_CREDENTIAL=github-copilot` selects the optional Pi auth-store route; the default is OpenRouter. Model IDs can be overridden with `PI_FIXTURE_OPENROUTER_MODEL`, `PI_FIXTURE_OPENROUTER_ANTHROPIC_MODEL`, and `PI_FIXTURE_COPILOT_ANTHROPIC_MODEL`.

After regeneration, run:

```sh
cargo test -p pi-ai --test m4_1_ordered_json
```

That test checks the case/file inventories, pinned provenance, compact ordered request bytes, retained turn-two lowering, replay markers, stable-header filtering, secret redaction, live-capture report, and every recorded digest.
