# Provider adapter crate stack and sourcing plan

The Go `ai` and `agent` packages are the reference: everything must be
faithfully ported to Rust. These ports are greenfield — no prior art is retained
from the existing Rust `genai-agent` and `ai` crates, which are not the intended
designs and are explicitly discarded. The forked `genai-agent` will be deleted,
as it is no longer needed.

The Rust `ai` crate is a complete and faithful port of the Go `ai` package: it
reproduces that package's behavior and provider wire semantics exactly. Once the
port is complete, it becomes the foundation for the next stage — completely
rebuilding the `agent` crate as a complete, faithful port of the Go `agent`
package.

This document records the crate and sourcing decisions for that port's provider
adapters. Faithfulness is the governing principle: crates are adopted where they
cover the surface we use faithfully, and owned (lifted and maintained) where
faithfulness requires exact control over the wire.

Reference versions are the snapshot the decisions were made against
(August 2026); pin to these and re-check for newer releases at implementation
time.

## The stack

| Adapter surface | Crate | Mode | License |
|---|---|---|---|
| OpenAI + all OpenAI-compatible providers | `async-openai` 0.41.x | Adopt as dependency | MIT |
| Azure OpenAI | `async-openai` `AzureConfig` | Adopt as dependency | MIT |
| Amazon Bedrock (Converse) | `aws-sdk-bedrockruntime` ≥1.140 + `aws-config` | Adopt as dependency | Apache-2.0 |
| Anthropic (first-party Messages) | own adapter, lifted from `adk-anthropic` 2.0 | Own (lift) | Apache-2.0 |
| Google (Gemini Developer API + Vertex) | own adapter, lifted from `adk-gemini` 2.0 | Own (lift) | Apache-2.0 |
| Google Vertex auth | `google-cloud-auth` | Adopt as dependency | Apache-2.0 |

Two modes are used deliberately:

- **Adopt as dependency** where a mature or official crate faithfully covers the
  surface we use.
- **Own (lift)** where we require exact control over the wire to stay faithful.
  We copy the relevant code into the crate and maintain it ourselves.

## OpenAI and OpenAI-compatible providers

**What.** Build the OpenAI Chat Completions and Responses adapters on
[`async-openai`](https://crates.io/crates/async-openai). The same adapters serve
every OpenAI-compatible provider (OpenRouter, xAI, DeepSeek, Mistral-compatible
paths, GitHub Copilot, Kimi, Cloudflare, Codex-compatible, and the rest of the
catalog) via custom base URL and per-request headers.

**Why.** `async-openai` provides typed, streaming coverage of both the Responses
API and Chat Completions — the two surfaces this layer uses — including tools and
streaming tool deltas, structured outputs, vision and file inputs, reasoning,
cached-token usage, custom base URL, and per-request headers. It is actively
maintained, so we consume it directly and keep its types private.

**How.**

- Use the Responses and Chat Completions clients; set a custom base URL and
  per-request headers for compatible providers.
- Enable the `byot` (bring-your-own-type) feature as the escape hatch for
  off-spec provider fields (for example `reasoning_content` on some
  OpenAI-compatible chat deltas): deserialize those responses into our own
  structs rather than the spec types.
- Map `async-openai` errors into our normalized error taxonomy.

## Azure OpenAI

**What.** Use `async-openai`'s `AzureConfig` for the Azure OpenAI Responses
adapter.

**Why.** Azure OpenAI is the OpenAI surface on an Azure endpoint with API-key
authentication, which `AzureConfig` supports natively — no Azure AD token flow is
required for this layer.

**How.** Configure `AzureConfig` with the resource base URL, `api-version`, the
deployment id, and the `AZURE_OPENAI_API_KEY` value. This sends the plain
`api-key` header and the `api-version` query parameter.

## Amazon Bedrock

**What.** Adopt the official
[`aws-sdk-bedrockruntime`](https://crates.io/crates/aws-sdk-bedrockruntime) plus
`aws-config`, using the Converse API. All Bedrock models — including Claude —
flow through Converse.

**Why.** The Rust SDK is generated from the same service model as the Go SDK, so
it faithfully covers the Converse/ConverseStream surface we use: typed streaming
events, tools and tool choice, multimodal content, prompt caching (`cachePoint`),
reasoning content, and usage. It is maintained by AWS.

**How.**

- Use `converse` and `converse_stream`; consume the typed `ConverseStreamOutput`
  events and map them to normalized events.
- Authenticate through the `aws-config` credential chain, plus native
  `AWS_BEARER_TOKEN_BEDROCK` bearer-token support (auto-detected from the
  environment).
- If a bearer token must also be read from a shared profile, source it directly
  and pass it via the config builder's `bearer_token()`.

## Anthropic (first-party Messages)

**What.** Own the Anthropic Messages adapter, lifted from
[`adk-anthropic`](https://crates.io/crates/adk-anthropic), with
[`claudius`](https://github.com/rescrv/claudius) as an additional reference.

**Why.** This layer needs exact control over the Anthropic Messages wire —
request-time `anthropic-beta` header composition and faithful HTTP-status
reporting — so we own the adapter to guarantee that fidelity. `adk-anthropic`
already models the first-party Messages surface faithfully (typed streaming
events, tool use with streaming input JSON, extended and interleaved thinking,
prompt caching, citations, and token counting), which makes it the starting
point.

**How.**

- Take `adk-anthropic`'s Messages types, streaming events, tool use, thinking,
  caching, citations, and token-counting as the base.
- Add request-time header control: compose `anthropic-beta` from the active
  feature set (fine-grained tool streaming, interleaved thinking) and the
  OAuth/Claude-Code path (`claude-code`, `oauth`), and support setting and
  deleting arbitrary headers.
- Preserve the true numeric HTTP status (in particular, do not collapse `529`
  into the rate-limit path) so status maps cleanly into the normalized error.

## Google (Gemini Developer API and Vertex)

**What.** Own a single Google adapter, lifted from
[`adk-gemini`](https://crates.io/crates/adk-gemini) and run REST-only, with
[`gemini-rust`](https://github.com/flachesis/gemini-rust) as an additional
reference. One adapter serves both the Gemini Developer API and Vertex backends.

**Why.** The adapter must faithfully serve both backends with streaming and
preserve Gemini's exact wire shapes — the full `Content`/`Part` union and
verbatim `thoughtSignature` round-tripping. Owning it guarantees that fidelity
and keeps the dependency footprint small.

**How.**

- Lift `adk-gemini`'s backend abstraction with two backends: Studio (Gemini
  Developer API, `x-goog-api-key`) and Vertex (ADC / service-account OAuth, plus
  Vertex Express API-key).
- Keep its hand-rolled SSE `streamGenerateContent` (over `reqwest` +
  `eventsource-stream`) for both backends. Run REST-only: drop the optional gRPC
  `PredictionService` path and the heavy generated GAPIC dependency stack.
- Keep the faithful `Content`/`Part` serde model, with `thoughtSignature`
  preserved as an opaque, round-trippable value on every variant. Add a catch-all
  `Part` variant for forward-compatibility with new part kinds.
- Sever the one intra-repo helper dependency (`adk-core`'s schema utilities) by
  either keeping `adk-core` as a normal crate dependency or copying its two
  self-contained schema files.

## Authentication (Google)

**What.** Use the official
[`google-cloud-auth`](https://crates.io/crates/google-cloud-auth) for the Vertex
backend (ADC, service account, workload identity federation, and API key). The
Gemini Developer API path uses a plain API-key header and needs no auth crate.

**Why.** It is the official, correctness-critical auth implementation and mirrors
the Go crate's `cloud.google.com/go/auth`.

**How.** Wire `google-cloud-auth` into the Vertex backend. If, after going
REST-only, its dependency weight is too heavy for the embedded/FFI target,
fall back to the leaner [`gcp_auth`](https://crates.io/crates/gcp_auth).

## Sourcing and licensing

We lift code from the reference crates and own it in-crate. All crates in the
stack are permissively licensed and none is copyleft, so our crate may be
licensed independently; we only preserve upstream notices for the code we copy.

| Source crate | License |
|---|---|
| `async-openai` | MIT |
| `aws-sdk-bedrockruntime`, `aws-config`, `google-cloud-auth` | Apache-2.0 |
| `adk-anthropic`, `adk-gemini`, `adk-core`, `claudius` | Apache-2.0 |
| `gemini-rust` | MIT |

Attribution and compliance:

- Maintain a `THIRD_PARTY_NOTICES` file listing each crate we copy from (name,
  URL, copyright line, and full license text). This satisfies MIT's notice
  requirement and Apache-2.0 §4(a).
- For Apache-2.0 files we copy and modify, add a one-line header noting the source
  and that the file was modified (Apache-2.0 §4(b) and §4(c)). The `adk-rust`
  repositories ship no NOTICE file, so §4(d) is a no-op for the `adk-*` crates.
- For crates shipped as compiled dependencies (`async-openai`, `aws-sdk-*`,
  `google-cloud-auth`), generate a bundled license manifest in CI with
  `cargo-about` or `cargo-deny` so their license texts travel with the
  distributed artifact.
