//! Internal catalog and registration support shared by provider leaf crates.

#![deny(missing_docs)]

mod catalog;
mod registration;

pub use catalog::*;
pub use registration::*;
