//! Immutable session entry trees, lanes, operational records, recovery, and storage.
//!
//! This crate realizes Architecture v2 part 2 §7.2–§7.6. The append log is
//! authoritative; [`SessionState`] is a deterministic projection of that log.
//! Native storage accepts atomic mutation batches under an optimistic global
//! sequence and the core has no Tokio dependency.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`c49906ec77788625aacbdc53ebca6fbe65bd20f5`) is the reference for behavior.

#![deny(missing_docs)]

mod error;
mod ids;
mod reducer;
mod storage;
mod types;

pub use agentprism_ai::{LocalBoxFuture, SendBoxFuture};
pub use error::*;
pub use ids::*;
pub use reducer::*;
pub use storage::*;
pub use types::*;
