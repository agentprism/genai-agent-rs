//! Lossless replay values from Architecture v2 part 2 §1.2 and §1.4–§1.9.

use crate::{ApiId, ContentBlockId, ModelId, ProviderId, ReplayItemId, ReplayKind, ToolCallId};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Current persisted replay-envelope schema (Architecture v2 part 2 §1.2).
pub const REPLAY_ENVELOPE_SCHEMA_VERSION: u16 = 1;

/// Ordered provider-protocol artifacts attached to an assistant message
/// (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayEnvelope {
    /// Persisted replay schema version.
    pub schema_version: u16,
    /// Scope of the response that produced the replay items.
    pub source: ReplayScope,
    /// Items in provider output order.
    pub items: Vec<ReplayItem>,
}

impl ReplayEnvelope {
    /// Creates an empty version-one envelope for a response scope.
    pub fn new(source: ReplayScope) -> Self {
        Self {
            schema_version: REPLAY_ENVELOPE_SCHEMA_VERSION,
            source,
            items: Vec::new(),
        }
    }

    /// Returns all items targeting a content block, preserving provider order
    /// (Architecture v2 part 2 §1.5).
    pub fn items_for_block<'a>(
        &'a self,
        block_id: &ContentBlockId,
    ) -> impl Iterator<Item = &'a ReplayItem> + 'a {
        let block_id = block_id.clone();
        self.items
            .iter()
            .filter(move |item| item.target == ReplayTarget::ContentBlock(block_id.clone()))
    }

    /// Finds the first complete, applicable item of `kind` for a content block
    /// in provider order (Architecture v2 part 2 §1.4).
    pub fn complete_item_for_block(
        &self,
        block_id: &ContentBlockId,
        kind: impl AsRef<str>,
        target: &ReplayScope,
    ) -> Option<&ReplayItem> {
        let kind = kind.as_ref();
        self.items_for_block(block_id)
            .filter(|item| {
                item.kind.as_str() == kind && item.is_complete_and_applicable(&self.source, target)
            })
            .min_by_key(|item| item.ordinal)
    }

    /// Finds the first complete, applicable item with a target and kind in
    /// provider order (Architecture v2 part 2 §1.8).
    pub fn complete_item(
        &self,
        replay_target: &ReplayTarget,
        kind: impl AsRef<str>,
        target: &ReplayScope,
    ) -> Option<&ReplayItem> {
        let kind = kind.as_ref();
        self.items
            .iter()
            .filter(|item| {
                &item.target == replay_target
                    && item.kind.as_str() == kind
                    && item.is_complete_and_applicable(&self.source, target)
            })
            .min_by_key(|item| item.ordinal)
    }

    /// Tests completeness and applicability using this envelope's source scope
    /// (Architecture v2 part 2 §1.9 R3–R5).
    pub fn is_complete_and_applicable(&self, item: &ReplayItem, target: &ReplayScope) -> bool {
        item.is_complete_and_applicable(&self.source, target)
    }
}

/// Provider, API, and model scope of replay data (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayScope {
    /// Provider that produced the response.
    pub provider: ProviderId,
    /// API family that produced the response.
    pub api: ApiId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Concrete model that produced the response.
    pub produced_by_model: ModelId,
    /// Optional provider-protocol revision.
    pub protocol_revision: Option<String>,
}

impl ReplayScope {
    /// Creates a scope without a protocol revision.
    pub fn new(
        provider: impl Into<ProviderId>,
        api: impl Into<ApiId>,
        requested_model: impl Into<ModelId>,
        produced_by_model: impl Into<ModelId>,
    ) -> Self {
        Self {
            provider: provider.into(),
            api: api.into(),
            requested_model: requested_model.into(),
            produced_by_model: produced_by_model.into(),
            protocol_revision: None,
        }
    }

    fn permits(&self, target: &Self, applicability: ReplayApplicability) -> bool {
        match applicability {
            ReplayApplicability::ExactProviderApiModel => {
                self.provider == target.provider
                    && self.api == target.api
                    && self.produced_by_model == target.requested_model
            }
            ReplayApplicability::ExactProviderApi => {
                self.provider == target.provider && self.api == target.api
            }
            ReplayApplicability::ApiFamily => self.api == target.api,
        }
    }
}

/// One ordered opaque provider artifact (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayItem {
    /// Stable item identifier.
    pub id: ReplayItemId,
    /// Original provider output ordinal.
    pub ordinal: u32,
    /// Canonical or provider-output target of the artifact.
    pub target: ReplayTarget,
    /// Open artifact kind understood by an API-family encoder.
    pub kind: ReplayKind,
    /// Scope within which the artifact may be replayed.
    pub applicability: ReplayApplicability,
    /// Whether the provider finished emitting the artifact.
    pub completeness: ReplayCompleteness,
    /// Opaque payload retained without semantic interpretation by agent core.
    pub payload: OpaquePayload,
}

impl ReplayItem {
    /// Tests whether the item is complete and its source scope permits the
    /// target request scope (Architecture v2 part 2 §1.9 R2–R5).
    pub fn is_complete_and_applicable(&self, source: &ReplayScope, target: &ReplayScope) -> bool {
        self.completeness == ReplayCompleteness::Complete
            && source.permits(target, self.applicability)
    }

    /// Returns an opaque UTF-8 payload when that is its encoding.
    pub fn as_utf8(&self) -> Option<&str> {
        self.payload.as_utf8()
    }

    /// Returns opaque bytes when that is their encoding.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.payload.as_bytes()
    }

    /// Returns exact compatibility-serializer JSON bytes, or an encoding error
    /// if this item is not JSON bytes (Architecture v2 part 2 §1.5).
    pub fn json_bytes(&self) -> Result<&[u8], OpaquePayloadEncodingError> {
        self.payload.json_bytes()
    }
}

/// Location to which a replay item belongs (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ReplayTarget {
    /// Artifact applies to the assistant message as a whole.
    Message,
    /// Artifact applies to one stable canonical content block.
    ContentBlock(ContentBlockId),
    /// Artifact applies to one stable tool call.
    ToolCall(ToolCallId),
    /// Artifact applies to an API output item not reducible to block order.
    ProviderOutputItem {
        /// Original provider output index.
        output_index: u32,
    },
}

impl ReplayTarget {
    /// Creates a content-block replay target.
    pub fn content_block(id: impl Into<ContentBlockId>) -> Self {
        Self::ContentBlock(id.into())
    }

    /// Creates a tool-call replay target.
    pub fn tool_call(id: impl Into<ToolCallId>) -> Self {
        Self::ToolCall(id.into())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EncodedReplayTarget {
    Message,
    ContentBlock { id: ContentBlockId },
    ToolCall { id: ToolCallId },
    ProviderOutputItem { output_index: u32 },
}

impl Serialize for ReplayTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Message => EncodedReplayTarget::Message.serialize(serializer),
            Self::ContentBlock(id) => {
                EncodedReplayTarget::ContentBlock { id: id.clone() }.serialize(serializer)
            }
            Self::ToolCall(id) => {
                EncodedReplayTarget::ToolCall { id: id.clone() }.serialize(serializer)
            }
            Self::ProviderOutputItem { output_index } => EncodedReplayTarget::ProviderOutputItem {
                output_index: *output_index,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ReplayTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match EncodedReplayTarget::deserialize(deserializer)? {
            EncodedReplayTarget::Message => Self::Message,
            EncodedReplayTarget::ContentBlock { id } => Self::ContentBlock(id),
            EncodedReplayTarget::ToolCall { id } => Self::ToolCall(id),
            EncodedReplayTarget::ProviderOutputItem { output_index } => {
                Self::ProviderOutputItem { output_index }
            }
        })
    }
}

/// Scope constraint for replaying an item (Architecture v2 part 2 §1.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayApplicability {
    /// Replay only to the exact provider, API, and produced model.
    ExactProviderApiModel,
    /// Replay only to the exact provider and API family.
    ExactProviderApi,
    /// Replay to any provider registration for the same API family.
    ApiFamily,
}

/// Completion state of opaque replay data (Architecture v2 part 2 §1.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCompleteness {
    /// The provider finished the replay item and an encoder may use it.
    Complete,
    /// Streaming ended before the item finished; encoders must ignore it.
    Incomplete,
}

/// Opaque provider payload with an explicit JSON encoding
/// (Architecture v2 part 2 §1.2 and §1.4–§1.8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpaquePayload {
    /// Provider-owned UTF-8 text.
    Utf8(String),
    /// Provider-owned bytes encoded as base64 at JSON boundaries.
    Bytes(Vec<u8>),
    /// Exact compatibility-serializer JSON bytes encoded as base64 at JSON boundaries.
    JsonBytes(Vec<u8>),
}

impl OpaquePayload {
    /// Returns the UTF-8 payload for the UTF-8 variant.
    pub fn as_utf8(&self) -> Option<&str> {
        match self {
            Self::Utf8(value) => Some(value),
            Self::Bytes(_) | Self::JsonBytes(_) => None,
        }
    }

    /// Returns the byte payload for the bytes variant.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(value) => Some(value),
            Self::Utf8(_) | Self::JsonBytes(_) => None,
        }
    }

    /// Returns exact JSON bytes for the JSON-bytes variant.
    pub fn json_bytes(&self) -> Result<&[u8], OpaquePayloadEncodingError> {
        match self {
            Self::JsonBytes(value) => Ok(value),
            Self::Utf8(_) | Self::Bytes(_) => Err(OpaquePayloadEncodingError),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct EncodedOpaquePayload {
    encoding: OpaquePayloadEncoding,
    data: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OpaquePayloadEncoding {
    Utf8,
    BytesBase64,
    JsonBytesBase64,
}

impl Serialize for OpaquePayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = match self {
            Self::Utf8(data) => EncodedOpaquePayload {
                encoding: OpaquePayloadEncoding::Utf8,
                data: data.clone(),
            },
            Self::Bytes(data) => EncodedOpaquePayload {
                encoding: OpaquePayloadEncoding::BytesBase64,
                data: BASE64.encode(data),
            },
            Self::JsonBytes(data) => EncodedOpaquePayload {
                encoding: OpaquePayloadEncoding::JsonBytesBase64,
                data: BASE64.encode(data),
            },
        };
        encoded.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OpaquePayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = EncodedOpaquePayload::deserialize(deserializer)?;
        match encoded.encoding {
            OpaquePayloadEncoding::Utf8 => Ok(Self::Utf8(encoded.data)),
            OpaquePayloadEncoding::BytesBase64 => BASE64
                .decode(encoded.data)
                .map(Self::Bytes)
                .map_err(de::Error::custom),
            OpaquePayloadEncoding::JsonBytesBase64 => BASE64
                .decode(encoded.data)
                .map(Self::JsonBytes)
                .map_err(de::Error::custom),
        }
    }
}

/// Returned when a replay payload is requested through the wrong encoding
/// accessor (Architecture v2 part 2 §1.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpaquePayloadEncodingError;

impl std::fmt::Display for OpaquePayloadEncodingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("replay payload is not encoded as JSON bytes")
    }
}

impl std::error::Error for OpaquePayloadEncodingError {}

impl PartialEq<ContentBlockId> for ReplayTarget {
    fn eq(&self, other: &ContentBlockId) -> bool {
        matches!(self, Self::ContentBlock(id) if id == other)
    }
}
