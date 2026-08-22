//! Structured context-handoff reports from Architecture v2 part 2 §4.2 and
//! §4.4.

use crate::{
    ApiId, AssistantFinishReason, ContentBlockId, MessageId, ModelId, ReplayItemId, ReplayKind,
    ToolCallId,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Provider, API-family, and concrete-model identity used when deciding
/// whether provider replay artifacts remain applicable.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ModelFingerprint {
    /// Provider identity.
    pub provider: crate::ProviderId,
    /// API-family identity.
    pub api: ApiId,
    /// Concrete model identity.
    pub model: ModelId,
}

impl ModelFingerprint {
    /// Creates a model fingerprint.
    pub fn new(
        provider: impl Into<crate::ProviderId>,
        api: impl Into<ApiId>,
        model: impl Into<ModelId>,
    ) -> Self {
        Self {
            provider: provider.into(),
            api: api.into(),
            model: model.into(),
        }
    }
}

/// Open diagnostic reason explaining why an opaque replay item was removed.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplayDropReason(
    /// Stable namespaced reason value.
    pub String,
);

impl ReplayDropReason {
    /// Creates an open replay-drop reason.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the reason as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One observable semantic or replay loss introduced while projecting
/// canonical history to a target model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandoffChange {
    /// A failed or aborted assistant record remains durable but is omitted
    /// from the provider request view.
    FailedAssistantOmitted {
        /// Omitted assistant message.
        message_id: MessageId,
        /// Terminal reason that made the message invalid for provider replay.
        reason: AssistantFinishReason,
    },
    /// An unsupported user or tool-result image became placeholder text.
    ImageReplaced {
        /// Message containing the image.
        message_id: MessageId,
        /// Replaced image block.
        block_id: ContentBlockId,
        /// Placeholder text inserted into the provider view.
        placeholder: String,
    },
    /// An opaque provider replay artifact was not applicable to the target.
    OpaqueReplayDropped {
        /// Assistant message owning the artifact.
        message_id: MessageId,
        /// Dropped replay item.
        replay_item_id: ReplayItemId,
        /// API-family replay kind.
        kind: ReplayKind,
        /// Reason for the drop.
        reason: ReplayDropReason,
    },
    /// Provider-redacted reasoning could not be represented for the target.
    RedactedThinkingDropped {
        /// Assistant message containing the block.
        message_id: MessageId,
        /// Dropped thinking block.
        block_id: ContentBlockId,
    },
    /// Visible reasoning was downgraded to ordinary text.
    ThinkingConvertedToText {
        /// Assistant message containing the block.
        message_id: MessageId,
        /// Converted thinking block.
        block_id: ContentBlockId,
        /// Whether configured opening and closing tags were inserted.
        tagged: bool,
    },
    /// A provider-incompatible tool-call identifier was rewritten.
    ToolCallIdRewritten {
        /// Assistant message containing the tool call.
        message_id: MessageId,
        /// Source identifier.
        old: ToolCallId,
        /// Target-compatible identifier.
        new: ToolCallId,
    },
    /// Provider-specific tool-call replay metadata was removed.
    ToolSignatureDropped {
        /// Assistant message containing the tool call.
        message_id: MessageId,
        /// Tool call whose signature was removed.
        tool_call_id: ToolCallId,
    },
    /// A missing result was closed with Pi-compatible error content.
    SyntheticToolResultInserted {
        /// Tool call receiving the synthetic result.
        tool_call_id: ToolCallId,
        /// Tool name copied into the result.
        tool_name: String,
    },
    /// An empty block with no applicable replay data was omitted.
    EmptyBlockDropped {
        /// Assistant message containing the block.
        message_id: MessageId,
        /// Omitted block.
        block_id: ContentBlockId,
    },
}

/// Complete structured report for one context projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffReport {
    /// Source model identities represented in the canonical history.
    pub source_models: BTreeSet<ModelFingerprint>,
    /// Target provider, API family, and model.
    pub target: ModelFingerprint,
    /// Ordered changes made during projection.
    pub changes: Vec<HandoffChange>,
    /// Whether any change lost canonical or replay information.
    pub lossy: bool,
}

impl HandoffReport {
    /// Creates a report containing no projection changes.
    pub fn unchanged(target: ModelFingerprint) -> Self {
        Self {
            source_models: BTreeSet::new(),
            target,
            changes: Vec::new(),
            lossy: false,
        }
    }
}
