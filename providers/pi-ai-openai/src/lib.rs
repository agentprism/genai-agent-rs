//! OpenAI-compatible Chat Completions providers for `pi-ai`.
//!
//! DeepSeek and OpenRouter share one API-family handler while retaining their
//! own authentication, endpoint, compatibility, and pinned catalog data.

#![deny(missing_docs)]

mod catalog;
mod decoder;
mod handler;
mod openrouter_oauth;

pub use catalog::*;
pub use decoder::*;
pub use handler::*;
pub use openrouter_oauth::*;
