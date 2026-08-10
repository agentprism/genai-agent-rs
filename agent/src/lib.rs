#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod agent;
pub mod agent_loop;
pub mod config;
pub mod error;
pub mod event;
pub mod hooks;
pub mod message;
pub mod tool;
pub mod validate;

// The assistant/stream-function/pricing modules were relocated into the `genai` fork crate
// (`genai-agentprism`). Re-export the modules here so agent-side code's `crate::assistant::…`,
// `crate::assistant_accumulator::…`, `crate::assistant_stream::…`, `crate::stream_fn::…`, and
// `crate::pricing::…` paths keep resolving unchanged.
pub use genai::{assistant, assistant_accumulator, assistant_stream, pricing, stream_fn};

#[cfg(feature = "proxy")]
pub mod proxy;

#[cfg(feature = "testing")]
pub mod testing;

pub use agent::*;
pub use agent_loop::*;
pub use config::*;
pub use error::*;
pub use event::*;
pub use hooks::*;
pub use message::*;
pub use tool::*;
pub use validate::*;

// Flatten the relocated modules at this crate's root, reproducing the exact glob surface the
// agent crate exposed before the move (external consumers and agent-side `crate::TypeName`
// references keep resolving unchanged).
pub use genai::assistant::*;
pub use genai::assistant_accumulator::*;
pub use genai::assistant_stream::*;
pub use genai::pricing::*;
pub use genai::stream_fn::*;

#[cfg(feature = "proxy")]
pub use proxy::*;

// Frequently needed upstream request/model primitives are re-exported for API consumers.
pub use genai::ModelSpec;
pub use genai::chat::ChatOptions;
pub use tokio_util::sync::CancellationToken;
