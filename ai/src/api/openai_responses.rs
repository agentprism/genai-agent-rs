//! PORT TARGET ⇐ pi `src/api/openai-responses.ts`.
//!
//! pi transport truth at the reference pin: SDK end-to-end —
//! `client.responses.create(params, requestOptions).withResponse()`, SDK-parsed
//! `ResponseStreamEvent`s handed to the shared, non-I/O `processResponsesStream()`
//! (`openai_responses_shared`). SDK retries disabled; `retryProviderRequest()` wraps the
//! initial call only.
//!
//! Rust transport (ruled 2026-08-19): `openai-oxide`, as in `openai_completions` — its
//! Responses streaming surface, with the same raw-request escape hatch for shapes the
//! spec types cannot express.
