//! `genai` library - A client library for any AI provider.
//! See [examples/c00-readme.rs](./examples/c00-readme.rs)

// Allow this crate's own code (including the relocated agent-side modules below, which use
// `genai::…` paths internally) to refer to itself as `genai`. The published crate is named
// `genai-agentprism`, so its lib target is aliased to `genai` (see `[lib] name` in Cargo.toml)
// for external consumers; `extern crate self as genai` provides the matching in-crate alias.
extern crate self as genai;

// region:    --- Modules

mod support;

mod client;
mod common;
mod error;

// -- Flatten
pub use client::*;
pub use common::*;
pub use error::{BoxError, Error, Result};

// -- Public Modules
pub mod adapter;
pub mod chat;
pub mod embed;
pub mod resolver;
pub mod webc;

// -- Agent-side modules relocated from the `rust-genai-agent` crate. They form the streaming
// assistant/stream-function contract and its pricing helpers. Flattened at the crate root so the
// relocated code's intra-crate `crate::TypeName` references resolve, mirroring how the agent crate
// exposed them. The agent crate now re-exports these modules to keep its public surface unchanged.
pub mod assistant;
pub mod assistant_accumulator;
pub mod assistant_stream;
pub mod pricing;
pub mod stream_fn;

pub use assistant::*;
pub use assistant_accumulator::*;
pub use assistant_stream::*;
pub use pricing::*;
pub use stream_fn::*;

// -- OpenTelemetry GenAI instrumentation (feature `otel`, off by default)
#[cfg(feature = "otel")]
pub mod otel;

// -- Folded first-party crates, now feature-gated modules of this crate.
// `auth` (was the `rust-genai-auth` crate) is OAuth login / token cache / refresh, mirroring
// pi-ai's `auth/`. `codex` (was the `rust-genai-codex` crate) is the ChatGPT-plan Codex backend
// StreamFn, mirroring pi-ai's `api/openai-codex-responses.ts`. Both are off by default.
#[cfg(feature = "auth")]
pub mod auth;
#[cfg(feature = "codex")]
pub mod codex;

// endregion: --- Modules

// region:    --- TLS Backend Guard

// TLS backends are mutually exclusive (forwarded to reqwest; see Cargo.toml / README).
// Enabling `native-tls` without `default-features = false` leaves `rustls-tls` on from
// the default set; turn that silent mis-selection into a clear compile-time error.
// The "neither feature" case is intentionally allowed — it is the supported
// bring-your-own-client path (`with_reqwest`).
#[cfg(all(feature = "rustls-tls", feature = "native-tls"))]
compile_error!(
	"genai: `rustls-tls` and `native-tls` are mutually exclusive. \
	 To use native-tls, set `default-features = false` and enable `native-tls`."
);

// endregion: --- TLS Backend Guard
