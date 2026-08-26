//! Strong identifiers and the shared session-log sequence.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = "Creates an identifier from its open string value."]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[doc = "Returns the identifier as a string slice."]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = "Consumes the identifier and returns its string value."]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_id!(SessionId, "A stable durable session identifier.");
string_id!(EntryId, "A stable immutable session-entry identifier.");
string_id!(LaneName, "An open session-lane name.");
string_id!(OperationRecordId, "A stable operational-record identifier.");

/// One-based position in the session-wide append log.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Sequence(u64);

impl Sequence {
    /// Sequence before the first append.
    pub const ZERO: Self = Self(0);

    /// Sequence assigned to the first mutation.
    pub const FIRST: Self = Self(1);

    /// Creates a sequence from its integer representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the integer representation.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next sequence, or `None` at integer exhaustion.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for Sequence {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<Sequence> for u64 {
    fn from(value: Sequence) -> Self {
        value.get()
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
