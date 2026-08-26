//! Canonical context handoff and structured loss reports from Architecture v2
//! part 2 §4.2–§4.4.

use crate::{
    ApiId, AssistantFinishReason, AssistantMessage, ContentBlock, ContentBlockId, Context, Message,
    MessageId, Modality, ModelDescriptor, ModelId, ReplayItem, ReplayItemId, ReplayKind,
    ReplayScope, ReplayTarget, Timestamp, ToolCall, ToolCallId, ToolResultContent,
    ToolResultMessage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

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

    fn record(&mut self, change: HandoffChange, lossy: bool) {
        self.changes.push(change);
        self.lossy |= lossy;
    }
}

/// Whether a lossy provider projection is accepted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffLossPolicy {
    /// Return the projected context and its complete report.
    #[default]
    AllowAndReport,
    /// Reject after projection if canonical or replay information was lost.
    RejectLossy,
}

/// Downgrade behavior for visible thinking sent to another model.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingFallback {
    /// Convert thinking to ordinary untagged text, matching pinned Pi.
    #[default]
    PlainText,
    /// Convert thinking to text surrounded by caller-selected tags.
    TaggedText {
        /// Prefix inserted immediately before visible thinking.
        opening: String,
        /// Suffix inserted immediately after visible thinking.
        closing: String,
    },
    /// Remove visible thinking from the provider projection.
    Drop,
}

/// Downgrade behavior for images unsupported by the target model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFallback {
    /// Insert Pi's distinct user-image and tool-image placeholders.
    #[default]
    PlaceholderText,
    /// Remove unsupported images.
    Drop,
    /// Reject the handoff.
    Reject,
}

/// Closure behavior for retained tool calls without results.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanToolPolicy {
    /// Insert Pi's `No result provided` error result.
    #[default]
    SynthesizeErrorResult,
    /// Remove the orphaned call from the projected assistant message.
    DropCall,
    /// Reject the handoff.
    Reject,
}

/// Provider-view behavior for durable failed assistant records.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedTurnProjection {
    /// Omit failed and aborted assistants, matching pinned Pi.
    #[default]
    Omit,
    /// Retain only their visible display text and no replay/tool metadata.
    IncludeDisplayTextOnly,
}

/// Complete cross-provider projection policy (Architecture v2 part 2 §4.2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffPolicy {
    /// Strict-versus-reporting behavior.
    pub loss_policy: HandoffLossPolicy,
    /// Cross-model visible-thinking behavior.
    pub thinking_fallback: ThinkingFallback,
    /// Unsupported-image behavior.
    pub image_fallback: ImageFallback,
    /// Missing-tool-result behavior.
    pub orphan_tool_policy: OrphanToolPolicy,
    /// Failed and aborted assistant projection behavior.
    pub failed_turn_policy: FailedTurnProjection,
}

/// Result of all eight handoff phases.
#[derive(Clone, Debug, PartialEq)]
pub struct HandoffResult {
    /// Provider-neutral context accepted by the target API-family shaper.
    pub context: Context,
    /// Every compatibility change and loss made during projection.
    pub report: HandoffReport,
}

/// API-family tool-call identifier normalization hook
/// (Architecture v2 part 2 §4.3 phase 6).
pub trait ToolCallIdPolicy: Send + Sync {
    /// Produces a target-compatible identifier before collision repair.
    fn normalize(
        &self,
        original: &ToolCallId,
        source: &ModelFingerprint,
        target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError>;
}

/// API-family hooks whose concrete wire rules arrive with Milestone 4.
///
/// M3.4 owns canonical projection, replay filtering, identity closure, and the
/// structured report. API crates declare recognized replay kinds, supply their
/// ID normalizer, and perform phase-eight structural shaping through this seam.
pub trait ApiFamilyHandoff: Send + Sync {
    /// Whether the target encoder understands one replay kind.
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool;

    /// Target-specific tool-call identifier policy.
    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy;

    /// Performs API-family final shaping (phase 8).
    fn final_shape(
        &self,
        context: &mut Context,
        report: &mut HandoffReport,
    ) -> Result<(), HandoffError>;
}

/// Identity ID policy useful for APIs without identifier restrictions.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityToolCallIdPolicy;

impl ToolCallIdPolicy for IdentityToolCallIdPolicy {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        Ok(original.clone())
    }
}

/// Conservative pre-M4 hook: no replay kind is assumed understood and final
/// shaping is a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalApiFamilyHandoff;

impl ApiFamilyHandoff for CanonicalApiFamilyHandoff {
    fn recognizes_replay_kind(&self, _kind: &ReplayKind) -> bool {
        false
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        &IdentityToolCallIdPolicy
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

/// Failure from structural validation, strict handoff policy, or API shaping.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandoffError {
    /// A persisted canonical identifier was empty or duplicated.
    InvalidIdentity {
        /// Sanitized validation detail.
        message: String,
    },
    /// The target rejects unsupported images.
    UnsupportedImage {
        /// Message containing the image.
        message_id: MessageId,
        /// Unsupported image block.
        block_id: ContentBlockId,
    },
    /// A tool call remained open at a provider-view interruption.
    OrphanToolCall {
        /// Missing-result call.
        tool_call_id: ToolCallId,
    },
    /// Tool-call normalization could not produce a distinct valid identity.
    ToolCallIdCollision {
        /// Original source identity.
        tool_call_id: ToolCallId,
    },
    /// API-family phase-eight shaping rejected the context.
    ApiFinalShaping {
        /// Sanitized family validation detail.
        message: String,
    },
    /// Strict mode rejected the completed lossy projection.
    LossyProjection {
        /// Complete report explaining the rejection.
        report: Box<HandoffReport>,
    },
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIdentity { message } | Self::ApiFinalShaping { message } => {
                formatter.write_str(message)
            }
            Self::UnsupportedImage {
                message_id,
                block_id,
            } => write!(
                formatter,
                "target does not support image block {block_id} in message {message_id}"
            ),
            Self::OrphanToolCall { tool_call_id } => {
                write!(formatter, "tool call {tool_call_id} has no result")
            }
            Self::ToolCallIdCollision { tool_call_id } => write!(
                formatter,
                "tool call {tool_call_id} cannot be normalized without a collision"
            ),
            Self::LossyProjection { .. } => {
                formatter.write_str("strict handoff policy rejected a lossy projection")
            }
        }
    }
}

impl std::error::Error for HandoffError {}

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
const SYNTHETIC_TOOL_RESULT_TEXT: &str = "No result provided";

/// Applies the eight ordered handoff phases to a canonical context.
pub fn transform_context_for_model(
    context: &Context,
    target_model: &ModelDescriptor,
    policy: &HandoffPolicy,
    api: &dyn ApiFamilyHandoff,
) -> Result<HandoffResult, HandoffError> {
    // Phase 1: structural normalization and validation. Rust vectors are
    // canonicalized from legacy JSON null by the message deserializers.
    validate_context(context)?;

    let target = ModelFingerprint {
        provider: target_model.common.model_ref.provider.clone(),
        api: target_model.api.api_id(),
        model: target_model.common.model_ref.model.clone(),
    };
    let target_scope = ReplayScope::new(
        target.provider.clone(),
        target.api.clone(),
        target.model.clone(),
        target.model.clone(),
    );
    let supports_images = target_model
        .common
        .modalities
        .input
        .contains(&Modality::Image);
    let mut report = HandoffReport::unchanged(target.clone());

    for message in &context.messages {
        if let Message::Assistant(assistant) = message {
            report.source_models.insert(source_fingerprint(assistant));
        }
    }

    let mut messages = Vec::with_capacity(context.messages.len());
    for message in &context.messages {
        match message {
            // Phase 3: target capability downgrade.
            Message::User(user) => {
                let mut user = user.clone();
                if !supports_images {
                    let content = std::mem::take(&mut user.content);
                    user.content = downgrade_user_images(&user.id, content, policy, &mut report)?;
                }
                messages.push(Message::User(user));
            }
            Message::ToolResult(tool_result) => {
                let mut tool_result = tool_result.clone();
                if !supports_images {
                    let content = std::mem::take(&mut tool_result.content);
                    tool_result.content =
                        downgrade_tool_images(&tool_result.id, content, policy, &mut report)?;
                }
                messages.push(Message::ToolResult(tool_result));
            }
            Message::Assistant(assistant) => {
                messages.push(Message::Assistant(assistant.clone()));
            }
        }
    }

    // Phases 4 and 5: replay applicability followed by content downgrade.
    for message in &mut messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        if is_failed_assistant(assistant) {
            continue;
        }
        filter_replay(assistant, &target_scope, api, &mut report);
        downgrade_assistant_content(assistant, &target, policy, &mut report);
    }

    // Phase 6: tool-call identity normalization and result rewrite.
    normalize_tool_call_ids(
        &mut messages,
        &target,
        api.tool_call_id_policy(),
        &mut report,
    )?;

    // Phase 7 plus corrected phase 2: close prior pending calls at every
    // assistant boundary before projecting a failed assistant away. Pinned Pi
    // performs this closure before its failed-turn skip.
    messages = close_orphan_tool_calls(messages, policy, &mut report)?;

    let mut projected = Context {
        schema_version: context.schema_version,
        system_prompt: context.system_prompt.clone(),
        messages,
        tools: context.tools.clone(),
    };

    // Phase 8: API-family final shaping is intentionally an injected M4 hook.
    api.final_shape(&mut projected, &mut report)?;
    validate_context(&projected)?;

    if report.lossy && matches!(policy.loss_policy, HandoffLossPolicy::RejectLossy) {
        return Err(HandoffError::LossyProjection {
            report: Box::new(report),
        });
    }

    Ok(HandoffResult {
        context: projected,
        report,
    })
}

fn validate_context(context: &Context) -> Result<(), HandoffError> {
    let mut message_ids = BTreeSet::new();

    for message in &context.messages {
        if message.id().as_str().is_empty() || !message_ids.insert(message.id().clone()) {
            return Err(HandoffError::InvalidIdentity {
                message: format!("message id must be non-empty and unique: {}", message.id()),
            });
        }
        let mut block_ids = BTreeSet::new();
        match message {
            Message::User(message) => {
                for block in &message.content {
                    validate_block_id(block.id(), &mut block_ids)?;
                }
            }
            Message::ToolResult(message) => {
                for block in &message.content {
                    let id = match block {
                        ToolResultContent::Text { id, .. }
                        | ToolResultContent::Image { id, .. } => id,
                    };
                    validate_block_id(id, &mut block_ids)?;
                }
            }
            Message::Assistant(message) => {
                for block in &message.content {
                    validate_block_id(block.id(), &mut block_ids)?;
                    if let ContentBlock::ToolCall { call, .. } = block
                        && call.id.as_str().is_empty()
                    {
                        return Err(HandoffError::InvalidIdentity {
                            message: "tool-call id must be non-empty".to_owned(),
                        });
                    }
                }
                let mut replay_ids = BTreeSet::new();
                for item in &message.replay.items {
                    if item.id.as_str().is_empty() || !replay_ids.insert(item.id.clone()) {
                        return Err(HandoffError::InvalidIdentity {
                            message: format!(
                                "replay item id must be non-empty and unique: {}",
                                item.id
                            ),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_block_id(
    id: &ContentBlockId,
    block_ids: &mut BTreeSet<ContentBlockId>,
) -> Result<(), HandoffError> {
    if id.as_str().is_empty() || !block_ids.insert(id.clone()) {
        return Err(HandoffError::InvalidIdentity {
            message: format!("content-block id must be non-empty and unique: {id}"),
        });
    }
    Ok(())
}

fn source_fingerprint(message: &AssistantMessage) -> ModelFingerprint {
    ModelFingerprint {
        provider: message.provider.clone(),
        api: message.api.clone(),
        model: message
            .response_model
            .clone()
            .unwrap_or_else(|| message.requested_model.clone()),
    }
}

fn failed_display_only(
    assistant: &AssistantMessage,
    report: &mut HandoffReport,
) -> AssistantMessage {
    let mut result = assistant.clone();
    for item in &result.replay.items {
        record_replay_drop(report, &assistant.id, item, "failed_display_text_only");
    }
    result.replay.items.clear();
    result.content = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { .. } => Some(block.clone()),
            ContentBlock::Thinking {
                id,
                text,
                redacted: false,
                ..
            } if !text.trim().is_empty() => {
                report.record(
                    HandoffChange::ThinkingConvertedToText {
                        message_id: assistant.id.clone(),
                        block_id: id.clone(),
                        tagged: false,
                    },
                    true,
                );
                Some(ContentBlock::Text {
                    id: id.clone(),
                    text: text.clone(),
                })
            }
            ContentBlock::Thinking { id, redacted, .. } => {
                if *redacted {
                    report.record(
                        HandoffChange::RedactedThinkingDropped {
                            message_id: assistant.id.clone(),
                            block_id: id.clone(),
                        },
                        true,
                    );
                } else {
                    report.record(
                        HandoffChange::EmptyBlockDropped {
                            message_id: assistant.id.clone(),
                            block_id: id.clone(),
                        },
                        false,
                    );
                }
                None
            }
            ContentBlock::Image { id, .. } | ContentBlock::ToolCall { id, .. } => {
                report.record(
                    HandoffChange::EmptyBlockDropped {
                        message_id: assistant.id.clone(),
                        block_id: id.clone(),
                    },
                    true,
                );
                None
            }
        })
        .collect();
    result
}

fn is_failed_assistant(assistant: &AssistantMessage) -> bool {
    matches!(
        assistant.finish.reason,
        AssistantFinishReason::Error | AssistantFinishReason::Aborted
    )
}

fn downgrade_user_images(
    message_id: &MessageId,
    content: Vec<ContentBlock>,
    policy: &HandoffPolicy,
    report: &mut HandoffReport,
) -> Result<Vec<ContentBlock>, HandoffError> {
    let mut result = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        if let ContentBlock::Image { id, .. } = block {
            match policy.image_fallback {
                ImageFallback::Reject => {
                    return Err(HandoffError::UnsupportedImage {
                        message_id: message_id.clone(),
                        block_id: id,
                    });
                }
                ImageFallback::Drop => {
                    report.record(
                        HandoffChange::ImageReplaced {
                            message_id: message_id.clone(),
                            block_id: id,
                            placeholder: String::new(),
                        },
                        true,
                    );
                    previous_was_placeholder = false;
                }
                ImageFallback::PlaceholderText => {
                    report.record(
                        HandoffChange::ImageReplaced {
                            message_id: message_id.clone(),
                            block_id: id.clone(),
                            placeholder: NON_VISION_USER_IMAGE_PLACEHOLDER.to_owned(),
                        },
                        true,
                    );
                    if !previous_was_placeholder {
                        result.push(ContentBlock::Text {
                            id,
                            text: NON_VISION_USER_IMAGE_PLACEHOLDER.to_owned(),
                        });
                    }
                    previous_was_placeholder = true;
                }
            }
            continue;
        }
        previous_was_placeholder = matches!(
            &block,
            ContentBlock::Text { text, .. } if text == NON_VISION_USER_IMAGE_PLACEHOLDER
        );
        result.push(block);
    }
    Ok(result)
}

fn downgrade_tool_images(
    message_id: &MessageId,
    content: Vec<ToolResultContent>,
    policy: &HandoffPolicy,
    report: &mut HandoffReport,
) -> Result<Vec<ToolResultContent>, HandoffError> {
    let mut result = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        if let ToolResultContent::Image { id, .. } = block {
            match policy.image_fallback {
                ImageFallback::Reject => {
                    return Err(HandoffError::UnsupportedImage {
                        message_id: message_id.clone(),
                        block_id: id,
                    });
                }
                ImageFallback::Drop => {
                    report.record(
                        HandoffChange::ImageReplaced {
                            message_id: message_id.clone(),
                            block_id: id,
                            placeholder: String::new(),
                        },
                        true,
                    );
                    previous_was_placeholder = false;
                }
                ImageFallback::PlaceholderText => {
                    report.record(
                        HandoffChange::ImageReplaced {
                            message_id: message_id.clone(),
                            block_id: id.clone(),
                            placeholder: NON_VISION_TOOL_IMAGE_PLACEHOLDER.to_owned(),
                        },
                        true,
                    );
                    if !previous_was_placeholder {
                        result.push(ToolResultContent::Text {
                            id,
                            text: NON_VISION_TOOL_IMAGE_PLACEHOLDER.to_owned(),
                        });
                    }
                    previous_was_placeholder = true;
                }
            }
            continue;
        }
        previous_was_placeholder = matches!(
            &block,
            ToolResultContent::Text { text, .. } if text == NON_VISION_TOOL_IMAGE_PLACEHOLDER
        );
        result.push(block);
    }
    Ok(result)
}

fn filter_replay(
    assistant: &mut AssistantMessage,
    target_scope: &ReplayScope,
    api: &dyn ApiFamilyHandoff,
    report: &mut HandoffReport,
) {
    let source = assistant.replay.source.clone();
    let mut retained = Vec::with_capacity(assistant.replay.items.len());
    let mut reported_tool_signatures = BTreeSet::new();
    for item in assistant.replay.items.drain(..) {
        let reason = if !matches!(item.completeness, crate::ReplayCompleteness::Complete) {
            Some("incomplete")
        } else if !item.is_complete_and_applicable(&source, target_scope) {
            Some("not_applicable")
        } else if !api.recognizes_replay_kind(&item.kind) {
            Some("unrecognized_kind")
        } else {
            None
        };

        if let Some(reason) = reason {
            if let ReplayTarget::ToolCall(tool_call_id) = &item.target
                && reported_tool_signatures.insert(tool_call_id.clone())
            {
                report.record(
                    HandoffChange::ToolSignatureDropped {
                        message_id: assistant.id.clone(),
                        tool_call_id: tool_call_id.clone(),
                    },
                    true,
                );
            }
            record_replay_drop(report, &assistant.id, &item, reason);
        } else {
            retained.push(item);
        }
    }
    assistant.replay.items = retained;
}

fn downgrade_assistant_content(
    assistant: &mut AssistantMessage,
    target: &ModelFingerprint,
    policy: &HandoffPolicy,
    report: &mut HandoffReport,
) {
    let exact_model = source_fingerprint(assistant) == *target;
    let content = std::mem::take(&mut assistant.content);
    let mut result = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Thinking {
                id,
                text,
                redacted,
                replay_item,
            } => {
                let has_replay = assistant.replay.items.iter().any(|item| {
                    item.target == ReplayTarget::ContentBlock(id.clone())
                        || replay_item.as_ref() == Some(&item.id)
                });
                if redacted {
                    if exact_model && has_replay {
                        result.push(ContentBlock::Thinking {
                            id,
                            text,
                            redacted,
                            replay_item: replay_item.filter(|replay_id| {
                                assistant
                                    .replay
                                    .items
                                    .iter()
                                    .any(|item| &item.id == replay_id)
                            }),
                        });
                    } else {
                        report.record(
                            HandoffChange::RedactedThinkingDropped {
                                message_id: assistant.id.clone(),
                                block_id: id.clone(),
                            },
                            true,
                        );
                        drop_replay_for_thinking(
                            assistant,
                            &id,
                            replay_item.as_ref(),
                            report,
                            "redacted_thinking_dropped",
                        );
                    }
                } else if text.trim().is_empty() {
                    if has_replay {
                        result.push(ContentBlock::Thinking {
                            id,
                            text,
                            redacted,
                            replay_item: replay_item.filter(|replay_id| {
                                assistant
                                    .replay
                                    .items
                                    .iter()
                                    .any(|item| &item.id == replay_id)
                            }),
                        });
                    } else {
                        report.record(
                            HandoffChange::EmptyBlockDropped {
                                message_id: assistant.id.clone(),
                                block_id: id,
                            },
                            false,
                        );
                    }
                } else if exact_model {
                    result.push(ContentBlock::Thinking {
                        id,
                        text,
                        redacted,
                        replay_item: replay_item.filter(|replay_id| {
                            assistant
                                .replay
                                .items
                                .iter()
                                .any(|item| &item.id == replay_id)
                        }),
                    });
                } else {
                    drop_replay_for_thinking(
                        assistant,
                        &id,
                        replay_item.as_ref(),
                        report,
                        "thinking_converted_to_text",
                    );
                    match &policy.thinking_fallback {
                        ThinkingFallback::PlainText => {
                            report.record(
                                HandoffChange::ThinkingConvertedToText {
                                    message_id: assistant.id.clone(),
                                    block_id: id.clone(),
                                    tagged: false,
                                },
                                true,
                            );
                            result.push(ContentBlock::Text { id, text });
                        }
                        ThinkingFallback::TaggedText { opening, closing } => {
                            report.record(
                                HandoffChange::ThinkingConvertedToText {
                                    message_id: assistant.id.clone(),
                                    block_id: id.clone(),
                                    tagged: true,
                                },
                                true,
                            );
                            result.push(ContentBlock::Text {
                                id,
                                text: format!("{opening}{text}{closing}"),
                            });
                        }
                        ThinkingFallback::Drop => {
                            report.record(
                                HandoffChange::ThinkingConvertedToText {
                                    message_id: assistant.id.clone(),
                                    block_id: id,
                                    tagged: false,
                                },
                                true,
                            );
                        }
                    }
                }
            }
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::ToolCall { .. } => result.push(block),
        }
    }
    assistant.content = result;
}

fn drop_replay_for_thinking(
    assistant: &mut AssistantMessage,
    block_id: &ContentBlockId,
    replay_item_id: Option<&ReplayItemId>,
    report: &mut HandoffReport,
    reason: &str,
) {
    let mut retained = Vec::with_capacity(assistant.replay.items.len());
    for item in assistant.replay.items.drain(..) {
        if item.target == ReplayTarget::ContentBlock(block_id.clone())
            || replay_item_id == Some(&item.id)
        {
            record_replay_drop(report, &assistant.id, &item, reason);
        } else {
            retained.push(item);
        }
    }
    assistant.replay.items = retained;
}

fn record_replay_drop(
    report: &mut HandoffReport,
    message_id: &MessageId,
    item: &ReplayItem,
    reason: &str,
) {
    report.record(
        HandoffChange::OpaqueReplayDropped {
            message_id: message_id.clone(),
            replay_item_id: item.id.clone(),
            kind: item.kind.clone(),
            reason: ReplayDropReason::new(reason),
        },
        true,
    );
}

fn normalize_tool_call_ids(
    messages: &mut [Message],
    target: &ModelFingerprint,
    id_policy: &dyn ToolCallIdPolicy,
    report: &mut HandoffReport,
) -> Result<(), HandoffError> {
    // Pinned Pi retains one toolCallIdMap for the complete ordered first pass.
    // Results can therefore be rewritten after an intervening user or
    // assistant message. Only changed cross-model IDs enter that map, so a
    // later same-model or no-op occurrence leaves an earlier mapping intact.
    let ToolCallIdPlans {
        by_block: mut plans,
        reserved_final_ids: mut normalized_owners,
    } = plan_tool_call_ids(messages, target, id_policy)?;
    let mut old_to_new = BTreeMap::<ToolCallId, ToolCallId>::new();

    for message in messages.iter_mut() {
        match message {
            Message::Assistant(assistant) => {
                // Failed assistants participate in this first pass too when
                // they are cross-model: their changed mapping can rewrite a
                // later tool result even though the assistant itself is
                // omitted by the second pass.
                let mut assistant_rewrites = BTreeMap::<ToolCallId, ToolCallId>::new();
                for block in &mut assistant.content {
                    let ContentBlock::ToolCall { id, call } = block else {
                        continue;
                    };
                    let plan_key = (assistant.id.clone(), id.clone());
                    let Some(plan) = plans.remove(&plan_key) else {
                        return Err(HandoffError::InvalidIdentity {
                            message: format!(
                                "tool-call block {id} in message {} has no normalization plan",
                                assistant.id
                            ),
                        });
                    };
                    if !plan.rewrite {
                        continue;
                    }
                    let original = plan.original;
                    let base = plan.base;
                    let normalized = match normalized_owners.get(&base) {
                        // Repeating the same original ID is an overwrite, not
                        // a post-normalization collision.
                        None => base,
                        Some(owner) if owner == &original => base,
                        Some(_) => collision_safe_id(
                            &original,
                            &base,
                            &plan.source,
                            target,
                            id_policy,
                            &normalized_owners,
                        )?,
                    };
                    normalized_owners.insert(normalized.clone(), original.clone());
                    old_to_new.insert(original.clone(), normalized.clone());
                    assistant_rewrites.insert(original.clone(), normalized.clone());
                    report.record(
                        HandoffChange::ToolCallIdRewritten {
                            message_id: assistant.id.clone(),
                            old: original,
                            new: normalized.clone(),
                        },
                        false,
                    );
                    call.id = normalized;
                }

                for item in &mut assistant.replay.items {
                    if let ReplayTarget::ToolCall(id) = &mut item.target
                        && let Some(normalized) = assistant_rewrites.get(id)
                    {
                        *id = normalized.clone();
                    }
                }
            }
            Message::ToolResult(result) => {
                if let Some(normalized) = old_to_new.get(&result.tool_call_id) {
                    result.tool_call_id = normalized.clone();
                }
            }
            Message::User(_) => {}
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct PlannedToolCallId {
    source: ModelFingerprint,
    original: ToolCallId,
    base: ToolCallId,
    rewrite: bool,
}

struct ToolCallIdPlans {
    by_block: BTreeMap<(MessageId, ContentBlockId), PlannedToolCallId>,
    reserved_final_ids: BTreeMap<ToolCallId, ToolCallId>,
}

fn plan_tool_call_ids(
    messages: &[Message],
    target: &ModelFingerprint,
    id_policy: &dyn ToolCallIdPolicy,
) -> Result<ToolCallIdPlans, HandoffError> {
    let mut plans = BTreeMap::new();
    let mut unchanged_final_ids = BTreeMap::new();

    for message in messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        let source = source_fingerprint(assistant);
        let is_same_model = source == *target;

        for block in &assistant.content {
            let ContentBlock::ToolCall { id, call } = block else {
                continue;
            };
            let original = call.id.clone();
            let base = if is_same_model {
                original.clone()
            } else {
                id_policy.normalize(&original, &source, target)?
            };
            if base.as_str().is_empty() {
                return Err(HandoffError::InvalidIdentity {
                    message: format!("tool-call normalizer returned an empty id for {original}"),
                });
            }
            let rewrite = !is_same_model && base != original;

            // Phase 6 collision planning is independent of transcript order:
            // a changed call cannot claim the final ID of a later same-model
            // or cross-model no-op call. Failed assistants never retain tool
            // calls in either failed-turn projection mode, so their unchanged
            // IDs do not reserve final provider-view space.
            if !rewrite && !is_failed_assistant(assistant) {
                unchanged_final_ids
                    .entry(original.clone())
                    .or_insert_with(|| original.clone());
            }

            plans.insert(
                (assistant.id.clone(), id.clone()),
                PlannedToolCallId {
                    source: source.clone(),
                    original,
                    base,
                    rewrite,
                },
            );
        }
    }

    Ok(ToolCallIdPlans {
        by_block: plans,
        reserved_final_ids: unchanged_final_ids,
    })
}

fn collision_safe_id(
    original: &ToolCallId,
    base: &ToolCallId,
    source: &ModelFingerprint,
    target: &ModelFingerprint,
    id_policy: &dyn ToolCallIdPolicy,
    normalized_owners: &BTreeMap<ToolCallId, ToolCallId>,
) -> Result<ToolCallId, HandoffError> {
    for attempt in 0_u8..16 {
        let mut hasher = Sha256::new();
        hasher.update(original.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(source.provider.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(source.api.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(source.model.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(target.provider.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(target.api.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(target.model.as_str().as_bytes());
        hasher.update([attempt]);
        let digest = hasher.finalize();
        let hash = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = ToolCallId::new(format!("{hash}_{}", base.as_str()));
        let candidate = id_policy.normalize(&candidate, source, target)?;
        if !candidate.as_str().is_empty() {
            match normalized_owners.get(&candidate) {
                None => return Ok(candidate),
                Some(owner) if owner == original => return Ok(candidate),
                Some(_) => {}
            }
        }
    }
    Err(HandoffError::ToolCallIdCollision {
        tool_call_id: original.clone(),
    })
}

#[derive(Clone)]
struct PendingToolCall {
    assistant_id: MessageId,
    assistant_timestamp: Timestamp,
    block_id: ContentBlockId,
    call: ToolCall,
}

fn close_orphan_tool_calls(
    messages: Vec<Message>,
    policy: &HandoffPolicy,
    report: &mut HandoffReport,
) -> Result<Vec<Message>, HandoffError> {
    let mut result = Vec::with_capacity(messages.len());
    let mut pending = Vec::<PendingToolCall>::new();
    let mut existing_results = BTreeSet::<ToolCallId>::new();

    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                close_pending(
                    &mut result,
                    &mut pending,
                    &mut existing_results,
                    policy,
                    report,
                )?;
                if is_failed_assistant(&assistant) {
                    match policy.failed_turn_policy {
                        FailedTurnProjection::Omit => {
                            report.record(
                                HandoffChange::FailedAssistantOmitted {
                                    message_id: assistant.id,
                                    reason: assistant.finish.reason,
                                },
                                true,
                            );
                        }
                        FailedTurnProjection::IncludeDisplayTextOnly => {
                            result
                                .push(Message::Assistant(failed_display_only(&assistant, report)));
                        }
                    }
                    continue;
                }

                pending = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, call } => Some(PendingToolCall {
                            assistant_id: assistant.id.clone(),
                            assistant_timestamp: assistant.timestamp,
                            block_id: id.clone(),
                            call: call.clone(),
                        }),
                        ContentBlock::Text { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. } => None,
                    })
                    .collect();
                existing_results.clear();
                result.push(Message::Assistant(assistant));
            }
            Message::User(user) => {
                close_pending(
                    &mut result,
                    &mut pending,
                    &mut existing_results,
                    policy,
                    report,
                )?;
                result.push(Message::User(user));
            }
            Message::ToolResult(tool_result) => {
                existing_results.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
        }
    }
    close_pending(
        &mut result,
        &mut pending,
        &mut existing_results,
        policy,
        report,
    )?;
    Ok(result)
}

fn close_pending(
    result: &mut Vec<Message>,
    pending: &mut Vec<PendingToolCall>,
    existing_results: &mut BTreeSet<ToolCallId>,
    policy: &HandoffPolicy,
    report: &mut HandoffReport,
) -> Result<(), HandoffError> {
    let missing = pending
        .iter()
        .filter(|pending| !existing_results.contains(&pending.call.id))
        .cloned()
        .collect::<Vec<_>>();
    for pending in missing {
        match policy.orphan_tool_policy {
            OrphanToolPolicy::Reject => {
                return Err(HandoffError::OrphanToolCall {
                    tool_call_id: pending.call.id,
                });
            }
            OrphanToolPolicy::DropCall => {
                if let Some(Message::Assistant(assistant)) = result.iter_mut().find(|message| {
                    matches!(message, Message::Assistant(assistant) if assistant.id == pending.assistant_id)
                }) {
                    assistant.content.retain(|block| {
                        !matches!(block, ContentBlock::ToolCall { call, .. } if call.id == pending.call.id)
                    });
                    let mut retained = Vec::with_capacity(assistant.replay.items.len());
                    for item in assistant.replay.items.drain(..) {
                        if item.target == ReplayTarget::ToolCall(pending.call.id.clone()) {
                            record_replay_drop(
                                report,
                                &assistant.id,
                                &item,
                                "orphan_tool_call_dropped",
                            );
                        } else {
                            retained.push(item);
                        }
                    }
                    assistant.replay.items = retained;
                    report.record(
                        HandoffChange::EmptyBlockDropped {
                            message_id: assistant.id.clone(),
                            block_id: pending.block_id,
                        },
                        true,
                    );
                }
            }
            OrphanToolPolicy::SynthesizeErrorResult => {
                let digest = stable_synthetic_digest(&pending.assistant_id, &pending.call.id);
                result.push(Message::ToolResult(ToolResultMessage {
                    id: MessageId::new(format!("handoff-tool-result-{digest}")),
                    tool_call_id: pending.call.id.clone(),
                    tool_name: pending.call.name.clone(),
                    content: vec![ToolResultContent::Text {
                        id: ContentBlockId::new(format!("handoff-tool-result-content-{digest}")),
                        text: SYNTHETIC_TOOL_RESULT_TEXT.to_owned(),
                    }],
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    is_error: true,
                    timestamp: pending.assistant_timestamp,
                }));
                report.record(
                    HandoffChange::SyntheticToolResultInserted {
                        tool_call_id: pending.call.id,
                        tool_name: pending.call.name,
                    },
                    false,
                );
            }
        }
    }
    pending.clear();
    existing_results.clear();
    Ok(())
}

fn stable_synthetic_digest(message_id: &MessageId, tool_call_id: &ToolCallId) -> String {
    let mut hasher = Sha256::new();
    hasher.update(message_id.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(tool_call_id.as_str().as_bytes());
    hasher.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
