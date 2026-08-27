//! Provider-neutral contracts for Pi model execution.
//!
//! This first milestone supplies the canonical messages, replay envelope, model
//! descriptors, usage/pricing values, and credential value types from
//! Architecture v2 part 1 §3 and part 2 §1, §2.1, §5.1–§5.2, and §6.6.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`8fa7eebd235355522c8104166b4f1f959b4e2f10`) is the reference for behavior.

#![deny(missing_docs)]

mod anthropic_messages;
mod async_types;
mod auth;
mod bedrock;
mod cancellation;
mod catalog;
mod deferred;
mod estimate;
#[cfg(not(target_arch = "wasm32"))]
mod file_credentials;
mod google;
mod handoff;
mod ids;
mod images;
mod json_compat;
mod messages;
mod middleware;
mod mistral;
mod model;
mod models;
mod oauth;
mod openai_completions;
mod openai_responses;
mod openrouter_images;
mod options;
mod overflow;
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
pub use bedrock::*;
pub use cancellation::*;
pub use catalog::*;
pub use deferred::*;
pub use estimate::*;
#[cfg(not(target_arch = "wasm32"))]
pub use file_credentials::*;
pub use google::*;
pub use handoff::*;
pub use ids::*;
pub use images::*;
pub use json_compat::*;
pub use messages::*;
pub use middleware::*;
pub use mistral::*;
pub use model::*;
pub use models::*;
pub use oauth::*;
pub use openai_completions::*;
pub use openai_responses::*;
pub use openrouter_images::*;
pub use options::*;
pub use overflow::*;
pub use provider::*;
pub use replay::*;
pub use retry::*;
pub use runtime::*;
pub use sanitization::*;
pub use scripted::*;
pub use streaming::*;
pub use usage::*;
