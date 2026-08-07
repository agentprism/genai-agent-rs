#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod agent;
pub mod agent_loop;
pub mod assistant;
pub mod assistant_accumulator;
pub mod assistant_stream;
pub mod config;
pub mod error;
pub mod event;
pub mod hooks;
pub mod message;
pub mod stream_fn;
pub mod tool;
pub mod validate;

#[cfg(feature = "proxy")]
pub mod proxy;

#[cfg(feature = "testing")]
pub mod testing;

pub use agent::*;
pub use agent_loop::*;
pub use assistant::*;
pub use assistant_accumulator::*;
pub use assistant_stream::*;
pub use config::*;
pub use error::*;
pub use event::*;
pub use hooks::*;
pub use message::*;
pub use stream_fn::*;
pub use tool::*;
pub use validate::*;

#[cfg(feature = "proxy")]
pub use proxy::*;

// Frequently needed upstream request/model primitives are re-exported for API consumers.
pub use genai::ModelSpec;
pub use genai::chat::ChatOptions;
pub use tokio_util::sync::CancellationToken;
