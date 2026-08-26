//! Amazon Bedrock Converse Stream API family and provider leaf.

#![deny(missing_docs)]

mod auth;
mod catalog;
mod decoder;
mod handler;

pub use auth::{BedrockSigningConfig, BedrockStaticCredentials};
pub use catalog::*;
pub use decoder::*;
pub use handler::*;
