//! Shared Google API-family implementations and the Gemini provider leaf.

#![deny(missing_docs)]

mod auth;
mod catalog;
mod decoder;
mod handler;

pub use auth::*;
pub use catalog::*;
pub use decoder::*;
pub use handler::*;
