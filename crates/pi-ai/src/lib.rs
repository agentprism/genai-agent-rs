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

mod anthropic_messages;
mod async_types;
mod auth;
mod cancellation;
mod catalog;
mod estimate;
mod handoff;
mod ids;
mod json_compat;
mod messages;
mod middleware;
mod model;
mod models;
mod oauth;
mod openai_completions;
mod options;
mod provider;
mod replay;
mod retry;
mod runtime;
mod sanitization;
mod scripted;
mod streaming;
mod usage;

pub use anthropic_messages::*;
pub use async_types::*;
pub use auth::*;
pub use cancellation::*;
pub use catalog::*;
pub use estimate::*;
pub use handoff::*;
pub use ids::*;
pub use json_compat::*;
pub use messages::*;
pub use middleware::*;
pub use model::*;
pub use models::*;
pub use oauth::*;
pub use openai_completions::*;
pub use options::*;
pub use provider::*;
pub use replay::*;
pub use retry::*;
pub use runtime::*;
pub use sanitization::*;
pub use scripted::*;
pub use streaming::*;
pub use usage::*;
