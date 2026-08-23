//! Open identifiers from Architecture v2 part 1 §3.1 and part 2 §1.2.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(#[doc = "The open string value."] pub String);

        impl $name {
            #[doc = "Creates an identifier from an open string value."]
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

string_id!(
    ProviderId,
    "An open provider identifier (Architecture v2 part 1 §3.1)."
);
string_id!(
    ModelId,
    "An open provider model identifier (Architecture v2 part 1 §3.1)."
);
string_id!(
    ApiId,
    "An open API-family identifier (Architecture v2 part 1 §3.1)."
);
string_id!(
    ReplayKind,
    "An open replay-artifact kind (Architecture v2 part 2 §1.2)."
);
string_id!(
    ExtensionId,
    "A namespaced extension identifier (Architecture v2 part 2 §5.1)."
);
string_id!(
    MessageId,
    "A stable canonical message identifier (Architecture v2 part 2 §1.2)."
);
string_id!(
    ContentBlockId,
    "A stable canonical content-block identifier (Architecture v2 part 2 §1.2)."
);
string_id!(
    ToolCallId,
    "A stable provider-neutral tool-call identifier (Architecture v2 part 2 §1.2)."
);
string_id!(
    ReplayItemId,
    "A stable replay-item identifier (Architecture v2 part 2 §1.2)."
);
string_id!(
    RunId,
    "A stable agent-run identifier shared across crate boundaries (Architecture v2 part 1 §4.4)."
);
string_id!(
    AuthChallengeId,
    "A stable host-visible authentication challenge identifier (Architecture v2 part 2 §6.2)."
);

/// A provider/model lookup key (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider that owns the model.
    pub provider: ProviderId,
    /// Provider-scoped model identifier.
    pub model: ModelId,
}

impl ModelRef {
    /// Creates a provider/model reference.
    pub fn new(provider: impl Into<ProviderId>, model: impl Into<ModelId>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

/// A Unix timestamp in milliseconds (Architecture v2 part 1 §3.1).
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(
    /// Milliseconds since the Unix epoch.
    pub i64,
);

impl Timestamp {
    /// Creates a timestamp from Unix milliseconds.
    pub const fn from_unix_millis(value: i64) -> Self {
        Self(value)
    }

    /// Returns Unix milliseconds.
    pub const fn unix_millis(self) -> i64 {
        self.0
    }
}
