//! pi-agent-harness: compaction and branch summarization, skills and prompt templates, reference tools, telemetry, and orchestration over agent-core + session + environment. Architecture v2 part 2 §7.7–§7.12.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`8fa7eebd235355522c8104166b4f1f959b4e2f10`) is the reference for behavior.

#![deny(missing_docs)]

mod branch_summary;
mod compaction;
mod context;
mod error;
mod file_operations;
mod ids;
mod overflow;
mod prompt_templates;
mod reference_tools;
mod session;
mod skills;
mod telemetry;
mod truncation;

pub use branch_summary::*;
pub use compaction::*;
pub use context::*;
pub use error::*;
pub use ids::*;
pub use overflow::*;
pub use prompt_templates::*;
pub use reference_tools::*;
pub use session::*;
pub use skills::*;
pub use telemetry::*;
pub use truncation::*;
