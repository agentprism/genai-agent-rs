//! Internal catalog and registration support shared by provider leaf crates.

#![deny(missing_docs)]

mod catalog;
mod model_data;
mod registration;

pub use catalog::*;
pub use model_data::*;
pub use registration::*;
