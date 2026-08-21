//! Faithful Rust port of pi-ai (`@earendil-works/pi-ai`, reference commit
//! `496185f6e4267b979e3663c45f7eb70b0c6a97b4`).
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
pub mod auth;
pub mod bedrock_provider;
pub mod env_api_keys;
pub mod event_stream;
pub mod model_catalog;
pub mod models;
pub mod models_generated;
pub mod models_store;
pub mod providers;
pub mod session_resources;
pub mod types;
pub mod utils;

pub use api::anthropic_messages::{
    AnthropicEffort, AnthropicMessagesClient, AnthropicOptions, AnthropicThinkingDisplay,
};
pub use api::bedrock_converse_stream::{BedrockOptions, BedrockThinkingDisplay};
pub use api::google_generative_ai::{
    GoogleGenerativeAIApi, GoogleOptions, GoogleThinkingOptions, GoogleToolChoice,
};
pub use api::google_shared::{GoogleApiThinkingLevel, ResolvedGoogleThinkingLevel};
pub use api::google_vertex::{GoogleVertexApi, GoogleVertexOptions};
pub use auth::{context::*, credential_store::*, helpers::*, types::*};
pub use event_stream::*;
pub use models::*;
pub use models_store::*;
pub use providers::faux::*;
pub use types::*;
pub use utils::diagnostics::*;
pub use utils::json_parse::*;
pub use utils::overflow::*;
pub use utils::retry::*;
pub use utils::text::content_text;
pub use utils::typebox_helpers::*;
pub use utils::uuid::uuid_v7;
pub use utils::validation::*;
