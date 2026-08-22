//! pi-agent-core: the agent state machine over ModelRuntime, tools and tool scheduling, context projection policies, queue polling and lifecycle events. No provider catalog, credential store, filesystem, or process runtime. Architecture v2 part 1 §4; part 2 §2, §8, §9.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`c49906ec77788625aacbdc53ebca6fbe65bd20f5`) is the reference for behavior.

#![deny(missing_docs)]

mod control;
mod error;
mod events;
mod policy;
mod replay;
mod restore;
mod run;
mod state;
mod tools;

pub use control::*;
pub use error::*;
pub use events::*;
pub use policy::*;
pub use replay::*;
pub use restore::*;
pub use run::*;
pub use state::*;
pub use tools::*;
