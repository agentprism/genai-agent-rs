//! Shared OpenAI API-family implementations and the OpenAI provider leaf.

#![deny(missing_docs)]

mod azure;
mod catalog;
mod decoder;
mod handler;
mod responses_decoder;
mod responses_handler;

pub use azure::*;
pub use catalog::*;
pub use decoder::*;
pub use handler::*;
pub use responses_decoder::*;
pub use responses_handler::*;
