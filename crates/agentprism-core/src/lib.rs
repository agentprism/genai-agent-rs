//! pi-agent-core: the agent state machine over ModelRuntime, tools and tool scheduling, context projection policies, queue polling and lifecycle events. No provider catalog, credential store, filesystem, or process runtime. Architecture v2 part 1 §4; part 2 §2, §8, §9.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`8fa7eebd235355522c8104166b4f1f959b4e2f10`) is the reference for behavior.

#![deny(missing_docs)]

mod control;
mod error;
mod events;
mod policy;
mod replay;
mod restore;
mod run;
mod scheduler;
mod state;
mod tools;

pub use control::*;
pub use error::*;
pub use events::*;
pub use policy::*;
pub use replay::*;
pub use restore::*;
pub use run::*;
pub use scheduler::*;
pub use state::*;
pub use tools::*;
