# Pi provider request fixture corpus

This is the M4.1 capture corpus required by Architecture v2 part 2 §10.8. The hermetic 10-family × 28-case corpus was generated from the governing Pi commit `8fa7eebd235355522c8104166b4f1f959b4e2f10` on 2026-08-26.

The checked `anthropic-messages/`, `openai-completions/`, `openai-responses/`, `openai-codex-responses/`, `azure-openai-responses/`, `google-generative-ai/`, `google-vertex/`, `bedrock-converse-stream/`, `mistral-conversations/`, and `pi-messages/` trees are deterministic captures from Pi's real API-family modules. A local Bun server records the exact logical JSON request bytes and returns fixed SSE or provider event-stream frames. Pi assembles turn one, the tool appends a fixed tool result or user message, and **the same logical entrypoint (`stream` or `streamSimple`) is invoked for turn two**. Capture-time assertions reject a turn-two request that loses simple-option reasoning/thinking, sampling overlays, or max-output clamping. Azure receives the live ephemeral loopback endpoint only while executing; canonical `azureBaseUrl` provenance uses the stable valid URL `http://127.0.0.1:9`, and every Azure case is captured twice with a byte-for-byte artifact comparison. Codex requests are captured with `transport: "sse"`; the server decompresses Pi's level-three Zstandard HTTP entity before recording the byte-exact logical JSON body.

The additional `credential-backed/` tree retains provider frames originally captured through the same local server acting as a reverse proxy to live endpoints. Its request bodies are regenerated hermetically at the governing pin by replaying those stored, redacted frames; `providerResponseCapturedAtPiCommit` records the older frame-capture provenance separately. No live credential is needed to verify or refresh request-generation provenance.

The Rust tests validate corpus provenance, completeness, redaction, digests, and canonical `JSON.stringify` form. The M6.1 Responses conformance suite additionally performs byte-exact family encoding and encrypted-reasoning turn-two replay checks; the earlier OpenAI Completions and Anthropic Messages exact family comparisons remain tracked separately in the parity manifest.

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

All §10.8 cases are captured for each of the ten families:

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

The checked credential-backed corpus captured all four acceptance cases for the two M4.1 families:

- `openai-completions`: `text-only`, `one-tool-call`, `tool-results`, and `signed-thinking-replay`, using `OPENROUTER_API_KEY` with `openai/gpt-oss-20b`;
- `anthropic-messages`: the same four cases, using `OPENROUTER_API_KEY` with OpenRouter's Anthropic Messages endpoint and `anthropic/claude-haiku-4.5`.

No acceptance case remained unavailable. Before selecting the OpenRouter Anthropic endpoint, the same four Anthropic cases were attempted with an in-memory refresh of the Pi auth-store `github-copilot` credential. The account's enabled Claude model was rejected by the `/v1/messages` endpoint as `model_not_supported`; no GitHub credential was written or checked in.

The remaining §10.8 cases are present in the deterministic corpus but were not repeated against live credentials. They exercise synthetic compatibility branches, fake model/request headers, unsupported reasoning-level matrices, cache modes, cross-provider history, image payloads, and failure repair; repeating them would add live cost and provider-dependent failures without changing Pi's captured request-construction bytes. `credential-backed/report.json` is the machine-readable record of the live acceptance run.

`DEEPSEEK_API_KEY` was not selected because the OpenRouter model supplied the required structured reasoning replay. `GEMINI_API_KEY` targets a different API family. The Responses-family M6.1 corpus is hermetic, so it requires neither live credentials nor network access.

## Regeneration

From the capture-tool directory:

```sh
cd providers/fixtures/capture-tool
bun install --frozen-lockfile
PI_PIN_DIR=/home/vikash/pi-pin-8fa7eebd2 bun run capture
```

The tool verifies the pinned commit without spawning Git, copies only `packages/ai/src` into ignored scratch space, and loads the copy against the locked dependencies. It never modifies the Pi checkout.

To regenerate the request bodies around the checked credential-backed response frames without network or credentials:

```sh
PI_PIN_DIR=/home/vikash/pi-pin-8fa7eebd2 bun run capture:credential-replay
```

To perform a new live credential-backed acceptance capture instead:

```sh
PI_PIN_DIR=/home/vikash/pi-pin-8fa7eebd2 bun run capture:credential
```

`PI_FIXTURE_FAMILIES` and `PI_FIXTURE_CASES` accept comma-separated selections. `PI_FIXTURE_ANTHROPIC_CREDENTIAL=github-copilot` selects the optional Pi auth-store route; the default is OpenRouter. Model IDs can be overridden with `PI_FIXTURE_OPENROUTER_MODEL`, `PI_FIXTURE_OPENROUTER_ANTHROPIC_MODEL`, and `PI_FIXTURE_COPILOT_ANTHROPIC_MODEL`.

After regeneration, run:

```sh
cargo test -p agentprism-ai --test m4_1_ordered_json
```

That test checks the case/file inventories, pinned provenance, compact ordered request bytes, retained turn-two lowering, replay markers, stable-header filtering, secret redaction, credential-replay report, and every recorded digest.
