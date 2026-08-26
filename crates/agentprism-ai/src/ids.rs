//! Open identifiers from Architecture v2 part 1 §3.1 and part 2 §1.2.

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

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

/// UUIDv7 generation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UuidV7Error {
    /// The system clock precedes the Unix epoch or exceeds UUIDv7's 48-bit field.
    InvalidTimestamp,
    /// The host cryptographic random source failed.
    RandomSource {
        /// Sanitized host random-source diagnostic.
        message: String,
    },
}

impl fmt::Display for UuidV7Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimestamp => formatter.write_str("UUIDv7 timestamp is out of range"),
            Self::RandomSource { message } => {
                write!(formatter, "UUIDv7 random source failed: {message}")
            }
        }
    }
}

impl std::error::Error for UuidV7Error {}

#[derive(Clone, Copy, Debug)]
struct UuidV7State {
    timestamp: u64,
    sequence: u32,
}

/// Thread-safe RFC 9562 UUIDv7 generator with Pi-compatible monotonic ordering.
#[derive(Debug, Default)]
pub struct UuidV7Generator {
    state: Mutex<Option<UuidV7State>>,
}

impl UuidV7Generator {
    /// Creates an independent monotonic UUIDv7 sequence.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Generates an identifier from an injected timestamp and random block.
    ///
    /// This deterministic seam is useful to hosts with their own clock/random
    /// capabilities and to conformance tests. The first call seeds the 32-bit
    /// sequence from random bytes 6–9; same-or-earlier timestamps increment it.
    pub fn generate_at(
        &self,
        timestamp_millis: u64,
        random: [u8; 16],
    ) -> Result<String, UuidV7Error> {
        const MAX_TIMESTAMP: u64 = (1_u64 << 48) - 1;
        if timestamp_millis > MAX_TIMESTAMP {
            return Err(UuidV7Error::InvalidTimestamp);
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = match *state {
            Some(previous) if timestamp_millis <= previous.timestamp => {
                let sequence = previous.sequence.wrapping_add(1);
                let timestamp = if sequence == 0 {
                    previous
                        .timestamp
                        .checked_add(1)
                        .ok_or(UuidV7Error::InvalidTimestamp)?
                } else {
                    previous.timestamp
                };
                UuidV7State {
                    timestamp,
                    sequence,
                }
            }
            _ => UuidV7State {
                timestamp: timestamp_millis,
                sequence: u32::from_be_bytes([random[6], random[7], random[8], random[9]]),
            },
        };
        if current.timestamp > MAX_TIMESTAMP {
            return Err(UuidV7Error::InvalidTimestamp);
        }
        *state = Some(current);

        let mut bytes = random;
        let timestamp = current.timestamp.to_be_bytes();
        bytes[..6].copy_from_slice(&timestamp[2..]);
        bytes[6] = 0x70 | ((current.sequence >> 28) as u8 & 0x0f);
        bytes[7] = (current.sequence >> 20) as u8;
        bytes[8] = 0x80 | ((current.sequence >> 14) as u8 & 0x3f);
        bytes[9] = (current.sequence >> 6) as u8;
        bytes[10] = ((current.sequence as u8 & 0x3f) << 2) | (random[10] & 0x03);

        Ok(format_uuid(bytes))
    }
}

/// Generates a process-monotonic UUIDv7 using the host clock and random source.
pub fn uuid_v7() -> Result<String, UuidV7Error> {
    static GENERATOR: OnceLock<UuidV7Generator> = OnceLock::new();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| UuidV7Error::InvalidTimestamp)?
        .as_millis()
        .try_into()
        .map_err(|_| UuidV7Error::InvalidTimestamp)?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| UuidV7Error::RandomSource {
        message: error.to_string(),
    })?;
    GENERATOR
        .get_or_init(UuidV7Generator::new)
        .generate_at(timestamp, random)
}

fn format_uuid(bytes: [u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
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
