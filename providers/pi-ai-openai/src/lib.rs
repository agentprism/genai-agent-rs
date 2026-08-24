//! OpenAI-compatible Chat Completions providers for `pi-ai`.
//!
//! DeepSeek and OpenRouter share one API-family handler while retaining their
//! own authentication, endpoint, compatibility, and pinned catalog data.

#![deny(missing_docs)]

mod catalog;
mod codex_oauth;
mod decoder;
mod handler;
mod openrouter_oauth;
mod responses_decoder;
mod responses_handler;

pub use catalog::*;
pub use codex_oauth::*;
pub use decoder::*;
pub use handler::*;
pub use openrouter_oauth::*;
pub use responses_decoder::*;
pub use responses_handler::*;
