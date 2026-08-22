//! Durable agent state and persistence snapshots from Architecture v2 part 1
//! §4.2 and §4.9, revised by part 2 §8.1.

use pi_ai::{AssistantMessageSnapshot, Message, ModelRef, ReasoningLevel, ToolCallId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::value::RawValue;
use std::sync::Arc;

/// Current persisted [`AgentState`] schema.
pub const AGENT_STATE_SCHEMA_VERSION: u32 = 1;

/// Current persisted [`AgentSnapshot`] schema.
pub const AGENT_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// First event sequence allocated by a newly constructed agent.
pub const AGENT_INITIAL_SEQUENCE: u64 = 1;

/// Durable state owned by the agent state machine.
///
/// Streaming parser state, pending tool futures, queues, and policy scratch are
/// intentionally excluded so replay invariant R8 remains true.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    /// Persistence schema for this state value.
    pub schema_version: u32,
    /// System instruction used when preparing model context.
    pub system_prompt: String,
    /// Provider/model selected for future turns.
    pub model: ModelRef,
    /// Requested reasoning level, including explicit [`ReasoningLevel::Off`].
    pub reasoning: ReasoningLevel,
    /// Durable chronological agent transcript.
    pub transcript: Vec<AgentRecord>,
}

impl AgentState {
    /// Creates empty version-one agent state.
    pub fn new(
        system_prompt: impl Into<String>,
        model: ModelRef,
        reasoning: ReasoningLevel,
    ) -> Self {
        Self {
            schema_version: AGENT_STATE_SCHEMA_VERSION,
            system_prompt: system_prompt.into(),
            model,
            reasoning,
            transcript: Vec::new(),
        }
    }
}

/// One durable agent transcript record.
///
/// The explicit custom variant replaces TypeScript declaration merging with a
/// persistence- and FFI-safe tagged record.
#[derive(Clone, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Architecture v2 part 1 §4.2 specifies AgentRecord::Llm(Message) directly"
)]
pub enum AgentRecord {
    /// A provider-neutral message visible to context projection.
    Llm(Message),
    /// An application-defined durable record.
    Custom {
        /// Open custom kind registered by the restoring application.
        type_name: String,
        /// Exact JSON payload owned by that custom kind.
        payload: Box<RawValue>,
    },
}

#[derive(Serialize)]
struct TaggedAgentRecord<'a, T> {
    kind: &'static str,
    #[serde(flatten)]
    record: &'a T,
}

#[derive(Serialize, Deserialize)]
struct CustomAgentRecord {
    type_name: String,
    payload: Box<RawValue>,
}

impl Serialize for AgentRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Llm(message) => TaggedAgentRecord {
                kind: "llm",
                record: message,
            }
            .serialize(serializer),
            Self::Custom { type_name, payload } => TaggedAgentRecord {
                kind: "custom",
                record: &CustomAgentRecord {
                    type_name: type_name.clone(),
                    payload: payload.clone(),
                },
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AgentRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RecordTag {
            kind: String,
        }

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let tag = serde_json::from_str::<RecordTag>(raw.get()).map_err(de::Error::custom)?;
        match tag.kind.as_str() {
            "llm" => serde_json::from_str(raw.get())
                .map(Self::Llm)
                .map_err(de::Error::custom),
            "custom" => serde_json::from_str::<CustomAgentRecord>(raw.get())
                .map(|record| Self::Custom {
                    type_name: record.type_name,
                    payload: record.payload,
                })
                .map_err(de::Error::custom),
            other => Err(de::Error::unknown_variant(other, &["llm", "custom"])),
        }
    }
}

impl PartialEq for AgentRecord {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Llm(left), Self::Llm(right)) => left == right,
            (
                Self::Custom {
                    type_name: left_type,
                    payload: left_payload,
                },
                Self::Custom {
                    type_name: right_type,
                    payload: right_payload,
                },
            ) => left_type == right_type && left_payload.get() == right_payload.get(),
            (Self::Llm(_), Self::Custom { .. }) | (Self::Custom { .. }, Self::Llm(_)) => false,
        }
    }
}

impl AgentRecord {
    /// Returns the canonical message identifier for an LLM record.
    pub fn message_id(&self) -> Option<&pi_ai::MessageId> {
        match self {
            Self::Llm(message) => Some(message.id()),
            Self::Custom { .. } => None,
        }
    }

    /// Returns the custom kind name when this is a custom record.
    pub fn custom_type_name(&self) -> Option<&str> {
        match self {
            Self::Llm(_) => None,
            Self::Custom { type_name, .. } => Some(type_name),
        }
    }
}

/// Versioned complete observation of durable and active agent state.
///
/// The partial assistant snapshot is owned and scratch-free, so this complete
/// value can cross persistence and FFI boundaries. Pending calls are stable
/// identifiers only; executable futures are never serialized.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Persistence schema for this snapshot value.
    pub schema_version: u32,
    /// Durable agent state.
    pub state: AgentState,
    /// Sequence to allocate to the next event envelope.
    pub next_sequence: u64,
    /// Current partial or terminal assistant assembly observation.
    pub streaming: Option<AssistantMessageSnapshot>,
    /// Tool calls currently executing, in assistant source order.
    pub pending_tool_calls: Arc<[ToolCallId]>,
}

impl AgentSnapshot {
    /// Creates an idle version-one snapshot from durable state.
    pub fn new(state: AgentState) -> Self {
        Self {
            schema_version: AGENT_SNAPSHOT_SCHEMA_VERSION,
            state,
            next_sequence: AGENT_INITIAL_SEQUENCE,
            streaming: None,
            pending_tool_calls: Arc::from([]),
        }
    }
}
