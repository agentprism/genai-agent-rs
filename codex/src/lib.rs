//! `rust-genai-codex` — a [`StreamFn`] for the ChatGPT-subscription **Codex**
//! backend (`chatgpt.com/backend-api`).
//!
//! This is the Rust equivalent of pi-ai's
//! `packages/ai/src/api/openai-codex-responses.ts`. It is the transport that
//! bridges two auth-/transport-agnostic crates:
//!
//! - [`rust_genai_agent`] provides the [`StreamFn`](rust_genai_agent::StreamFn)
//!   trait and the `AssistantMessageEvent` protocol every stream function emits,
//!   but knows nothing about auth.
//! - [`rust_genai_auth`] owns the ChatGPT Codex OAuth flow, token cache, and
//!   refresh, but knows nothing about the Codex Responses wire protocol.
//!
//! [`CodexStreamFn`] needs **both** (a StreamFn that consumes OAuth tokens), so
//! it lives in its own crate to keep the other two decoupled.
//!
//! # What it does
//!
//! On each request it:
//! 1. resolves a **fresh bearer + `chatgpt-account-id`** from a [`TokenSource`]
//!    (pi refreshes per request; the auth crate's resolver handles
//!    expiry/refresh/persist with the double-refresh race fix),
//! 2. builds the OpenAI **Responses** request body from the agent's
//!    `LlmContext` + options,
//! 3. streams the response over **WebSocket with SSE fallback** (or SSE only),
//!    and
//! 4. maps the Codex Responses event stream onto the crate's assistant event
//!    protocol (start / text deltas / thinking / tool calls / done / error) via
//!    the same `AssistantAccumulator` that [`rust_genai_agent::GenaiStreamFn`]
//!    uses — so the `AssistantMessageEventStream` contract is identical.
//!
//! All failures (setup, transport, protocol, cancellation) are reported
//! **in-band** as a terminal error event; nothing is thrown.
//!
//! # Wiring (auth crate → CodexStreamFn → Agent)
//!
//! ```no_run
//! use std::sync::Arc;
//! use rust_genai_codex::{CodexStreamFn, StaticTokenSource};
//! use rust_genai_agent::Transport;
//!
//! // A token source: here a fixed token; production uses a refreshing resolver.
//! let token_source = Arc::new(StaticTokenSource::new("bearer-jwt", "acct_123"));
//! let stream_fn = CodexStreamFn::new(token_source)
//!     .with_transport(Transport::Auto); // WebSocket with SSE fallback
//! // Install `stream_fn` on an agent as its StreamFn (see rust-genai-agent).
//! let _ = stream_fn;
//! ```
//!
//! With the `auth-resolver` feature, build the token source from the auth crate's
//! `CodexTokenResolver` (fresh bearer + refresh + persist + account id from the
//! JWT); see [`ResolverTokenSource`].
//!
//! # Testing scope
//!
//! The test suite validates protocol framing, headers, event mapping, transport
//! fallback, and cancellation against **local mock servers only**. End-to-end
//! validation against the real ChatGPT backend is intentionally **out of scope
//! for CI**: it needs a real ChatGPT subscription, live OAuth, and network. See
//! the README/NOTES for details.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod events;
pub mod protocol;
pub mod request;
pub mod stream;
pub mod token;

pub use error::{CodexError, Result};
pub use protocol::{
    DEFAULT_CODEX_BASE_URL, DEFAULT_ORIGINATOR, JWT_CLAIM_PATH, OPENAI_BETA_SSE, OPENAI_BETA_WS,
};
pub use request::BodyConfig;
pub use stream::CodexStreamFn;
pub use token::{CodexToken, StaticTokenSource, TokenSource};

#[cfg(feature = "auth-resolver")]
pub use token::ResolverTokenSource;

// Re-export the agent transport advisory for convenience at the wiring site.
pub use rust_genai_agent::Transport;
