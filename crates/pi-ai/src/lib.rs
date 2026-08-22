//! Provider-neutral contracts for Pi model execution.
//!
//! This first milestone supplies the canonical messages, replay envelope, model
//! descriptors, usage/pricing values, and credential value types from
//! Architecture v2 part 1 §3 and part 2 §1, §2.1, §5.1–§5.2, and §6.6.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`c49906ec77788625aacbdc53ebca6fbe65bd20f5`) is the reference for behavior.

#![deny(missing_docs)]

mod async_types;
mod auth;
mod cancellation;
mod handoff;
mod ids;
mod messages;
mod model;
mod options;
mod replay;
mod runtime;
mod sanitization;
mod scripted;
mod streaming;
mod usage;

pub use async_types::*;
pub use auth::*;
pub use cancellation::*;
pub use handoff::*;
pub use ids::*;
pub use messages::*;
pub use model::*;
pub use options::*;
pub use replay::*;
pub use runtime::*;
pub use sanitization::*;
pub use scripted::*;
pub use streaming::*;
pub use usage::*;
