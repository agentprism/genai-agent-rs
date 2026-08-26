//! Immutable session entry trees, lanes, operational records, recovery, and storage.
//!
//! This crate realizes Architecture v2 part 2 §7.2–§7.6. The append log is
//! authoritative; [`SessionState`] is a deterministic projection of that log.
//! Native storage accepts atomic mutation batches under an optimistic global
//! sequence and the core has no Tokio dependency.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`8fa7eebd235355522c8104166b4f1f959b4e2f10`) is the reference for behavior.

#![deny(missing_docs)]

mod error;
mod file;
mod ids;
mod reducer;
mod search;
mod storage;
mod types;

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;

pub use agentprism_ai::{LocalBoxFuture, SendBoxFuture};
pub use error::*;
pub use file::*;
pub use ids::*;
pub use reducer::*;
pub use search::*;
pub use storage::*;
pub use types::*;
