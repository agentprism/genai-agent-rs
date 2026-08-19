//! PORT TARGET ⇐ pi `src/api/openai-completions.ts`.
//!
//! pi transport truth at the reference pin (provider-api-implementations.mdx): SDK
//! end-to-end — `client.chat.completions.create(params, requestOptions).withResponse()`,
//! SDK-parsed `ChatCompletionChunk` stream, SDK retries forced to zero,
//! `retryProviderRequest()` wrapping the initial call only. Compat-flag-parameterized for
//! every OpenAI-compatible provider (seam #9) — one module, N provider identities.
//!
//! Rust transport (ruled 2026-08-19): `openai-oxide` 0.16.x (crates.io; MIT;
//! github.com/fortunto2/openai-oxide), playing the role pi's `openai` SDK plays — client,
//! transport, SSE framing — with custom base URL and per-request headers. Wire shapes its
//! spec-synced types cannot express faithfully (off-spec compat fields, presence semantics)
//! go through its raw request access, with the types owned by this crate.
