//! Faithful Rust port of pi-ai (`@earendil-works/pi-ai`, reference: `~/pi/packages/ai` at
//! latest main).
//!
//! Governing rule: a consumer of this crate finds no feature or behavior difference from
//! pi-ai beyond language semantics. The preserved architectural seams are recorded in
//! `docs/porting-pi-ai-and-agent-core-docs/v2/preserved-architectural-seams-pi-ai-v2.mdx`;
//! the ported API-implementation subset is recorded in
//! `docs/porting-pi-ai-and-agent-core-docs/provider-api-implementations.mdx`.
//!
//! Layout mirrors pi-ai's `src/` file for file (snake_case where module rules force it):
//! `types.rs` ⇐ `types.ts`, `event_stream.rs` ⇐ `utils/event-stream.ts`,
//! `api/<name>.rs` ⇐ `api/<name>.ts`.

pub mod api;
pub mod event_stream;
pub mod types;
pub mod utils;

pub use event_stream::*;
pub use types::*;
