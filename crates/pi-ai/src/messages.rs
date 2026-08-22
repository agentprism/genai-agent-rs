//! Canonical messages and terminal status from Architecture v2 part 1 §3.1
//! and part 2 §1.2 and §2.1.

use crate::{
    ApiId, ContentBlockId, MessageId, ModelId, ProviderId, ReplayEnvelope, ReplayItemId, Timestamp,
    ToolCallId, Usage, VersionedExtension,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;

/// Current persisted conversation schema (Architecture v2 part 1 §3.1).
pub const CONVERSATION_SCHEMA_VERSION: u32 = 1;

/// Current persisted context schema (Architecture v2 part 1 §3.1).
pub const CONTEXT_SCHEMA_VERSION: u32 = 1;

/// A provider-neutral transcript message (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "Architecture v2 part 1 §3.1 specifies direct canonical message variants"
)]
pub enum Message {
    /// User-authored input.
    User(UserMessage),
    /// Model-authored output with replay and terminal metadata.
    Assistant(AssistantMessage),
    /// Result of executing a model-requested tool.
    ToolResult(ToolResultMessage),
}

#[derive(Serialize)]
struct TaggedMessage<'a, T> {
    role: &'static str,
    #[serde(flatten)]
    message: &'a T,
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::User(message) => TaggedMessage {
                role: "user",
                message,
            }
            .serialize(serializer),
            Self::Assistant(message) => TaggedMessage {
                role: "assistant",
                message,
            }
            .serialize(serializer),
            Self::ToolResult(message) => TaggedMessage {
                role: "tool_result",
                message,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct MessageTag {
            role: String,
        }

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let tag = serde_json::from_str::<MessageTag>(raw.get()).map_err(de::Error::custom)?;
        match tag.role.as_str() {
            "user" => serde_json::from_str(raw.get())
                .map(Self::User)
                .map_err(de::Error::custom),
            "assistant" => serde_json::from_str(raw.get())
                .map(Self::Assistant)
                .map_err(de::Error::custom),
            "tool_result" => serde_json::from_str(raw.get())
                .map(Self::ToolResult)
                .map_err(de::Error::custom),
            other => Err(de::Error::unknown_variant(
                other,
                &["user", "assistant", "tool_result"],
            )),
        }
    }
}

impl Message {
    /// Returns the stable message identifier.
    pub fn id(&self) -> &MessageId {
        match self {
            Self::User(message) => &message.id,
            Self::Assistant(message) => &message.id,
            Self::ToolResult(message) => &message.id,
        }
    }
}

/// Canonical user input (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Stable message identifier.
    pub id: MessageId,
    /// Ordered user content. Provider projection validates allowed block kinds.
    pub content: Vec<ContentBlock>,
    /// Creation time in Unix milliseconds.
    pub timestamp: Timestamp,
}

/// Canonical replay-aware model output (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Stable message identifier used from stream start through termination.
    pub id: MessageId,
    /// Provider that served the request.
    pub provider: ProviderId,
    /// API family that served the request.
    pub api: ApiId,
    /// Model requested by the caller.
    pub requested_model: ModelId,
    /// Concrete response model when reported.
    pub response_model: Option<ModelId>,
    /// Provider response identifier when reported.
    pub response_id: Option<String>,
    /// Ordered displayable and executable canonical content.
    pub content: Vec<ContentBlock>,
    /// Ordered opaque replay artifacts.
    pub replay: ReplayEnvelope,
    /// Last authoritative cumulative response usage.
    pub usage: Usage,
    /// Terminal finish metadata.
    pub finish: AssistantFinish,
    /// Creation time in Unix milliseconds.
    pub timestamp: Timestamp,
}

/// Canonical tool-result transcript message (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// Stable message identifier.
    pub id: MessageId,
    /// Tool call to which this result responds.
    pub tool_call_id: ToolCallId,
    /// Tool name retained for APIs that require it.
    pub tool_name: String,
    /// Ordered text and image result blocks.
    pub content: Vec<ToolResultContent>,
    /// Versioned tool-owned details not sent to providers by default.
    pub details: Option<VersionedExtension>,
    /// Usage attributable to tool execution rather than the model response.
    pub usage: Option<Usage>,
    /// Tool names made available after this result.
    pub added_tool_names: Vec<String>,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Creation time in Unix milliseconds.
    pub timestamp: Timestamp,
}

/// Stable provider-neutral content block (Architecture v2 part 2 §1.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Visible text.
    Text {
        /// Stable block identifier.
        id: ContentBlockId,
        /// Visible UTF-8 content.
        text: String,
    },
    /// Base64-encoded image data.
    Image {
        /// Stable block identifier.
        id: ContentBlockId,
        /// Base64 image bytes without a data-URL prefix.
        data: String,
        /// Image media type such as `image/png`.
        mime_type: String,
    },
    /// Visible or redacted reasoning.
    Thinking {
        /// Stable block identifier.
        id: ContentBlockId,
        /// Visible reasoning or the provider redaction placeholder.
        text: String,
        /// Whether safety or provider policy redacted the reasoning.
        redacted: bool,
        /// Optional reverse link shown by the persisted representation in part
        /// 2 §1.4. The replay envelope remains authoritative.
        #[serde(rename = "replayItem", skip_serializing_if = "Option::is_none")]
        replay_item: Option<ReplayItemId>,
    },
    /// Model-requested tool invocation.
    ToolCall {
        /// Stable canonical block identifier.
        id: ContentBlockId,
        /// Finalized tool-call semantics.
        call: ToolCall,
    },
}

impl ContentBlock {
    /// Returns the stable block identifier.
    pub fn id(&self) -> &ContentBlockId {
        match self {
            Self::Text { id, .. }
            | Self::Image { id, .. }
            | Self::Thinking { id, .. }
            | Self::ToolCall { id, .. } => id,
        }
    }
}

/// Finalized provider-neutral tool call (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Stable call identifier referenced by tool-result messages.
    pub id: ToolCallId,
    /// Tool name requested by the model.
    pub name: String,
    /// Final parsed JSON arguments. Streaming scratch fragments are never stored here.
    pub arguments: Value,
}

/// Provider-neutral model-visible tool specification (Architecture v2 part 1 §4.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Schema version of the persisted tool specification.
    pub schema_version: u32,
    /// Tool name exposed to the model.
    pub name: String,
    /// Human-readable model-facing description.
    pub description: String,
    /// JSON Schema for tool arguments.
    pub parameters: Value,
    /// Optional provider-side constrained-sampling contract.
    ///
    /// `None` omits the preference, while [`ConstrainedSampling::Disabled`]
    /// records Pi's explicit `false` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSampling>,
}

/// Pi's optional provider-side constrained-sampling value
/// (Architecture v2 part 1 §4.5; pinned Pi `types.ts`).
///
/// The disabled variant serializes as JSON `false`; configured variants use
/// the tagged [`ConstrainedSamplingConfig`] object shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConstrainedSampling {
    /// Explicitly disable constrained sampling for this tool.
    Disabled,
    /// Request one of Pi's typed constrained-sampling configurations.
    Config(ConstrainedSamplingConfig),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum EncodedConstrainedSampling {
    Disabled(bool),
    Config(ConstrainedSamplingConfig),
}

impl Serialize for ConstrainedSampling {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Config(config) => config.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ConstrainedSampling {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match EncodedConstrainedSampling::deserialize(deserializer)? {
            EncodedConstrainedSampling::Disabled(false) => Ok(Self::Disabled),
            EncodedConstrainedSampling::Disabled(true) => Err(de::Error::custom(
                "constrained sampling boolean must be false",
            )),
            EncodedConstrainedSampling::Config(config) => Ok(Self::Config(config)),
        }
    }
}

/// A configured provider-side constrained-sampling mode
/// (Architecture v2 part 1 §4.5; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    /// Ask the provider to enforce the tool's JSON Schema when possible or
    /// require enforcement according to `strict`.
    JsonSchema {
        /// Whether strict JSON-schema sampling is preferred or mandatory.
        strict: JsonSchemaStrictMode,
    },
    /// Supply provider-specific grammar encodings of the same language.
    Grammar {
        /// Available OpenAI grammar variants, keyed by their wire format.
        variants: GrammarVariants,
    },
}

/// Strict JSON-schema constrained-sampling policy from Pi's tool contract
/// (Architecture v2 part 1 §4.5; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonSchemaStrictMode {
    /// Prefer strict sampling, but allow a safe ordinary-tool fallback.
    Prefer,
    /// Require strict sampling and reject an unsupported or unsafe schema.
    Require,
}

/// OpenAI grammar format identifier from Pi's constrained-sampling contract
/// (Architecture v2 part 1 §4.5; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum GrammarFormat {
    /// OpenAI Lark grammar syntax (`openai_lark`).
    #[serde(rename = "openai_lark")]
    OpenAiLark,
    /// OpenAI regular-expression grammar syntax (`openai_regex`).
    #[serde(rename = "openai_regex")]
    OpenAiRegex,
}

/// Available provider grammar encodings, preserving Pi's two known formats
/// (Architecture v2 part 1 §4.5; pinned Pi `types.ts`).
pub type GrammarVariants = BTreeMap<GrammarFormat, String>;

/// Model-visible tool-result content (Architecture v2 part 1 §4.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    /// Text tool output.
    Text {
        /// Stable result-block identifier.
        id: ContentBlockId,
        /// UTF-8 output.
        text: String,
    },
    /// Image tool output.
    Image {
        /// Stable result-block identifier.
        id: ContentBlockId,
        /// Base64 image bytes without a data-URL prefix.
        data: String,
        /// Image media type.
        mime_type: String,
    },
}

/// Versioned durable conversation (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    /// Persisted conversation schema version.
    pub schema_version: u32,
    /// Optional provider-neutral system instruction.
    pub system_prompt: Option<String>,
    /// Durable transcript in chronological order.
    pub messages: Vec<Message>,
}

impl Conversation {
    /// Creates an empty version-one conversation.
    pub fn new(system_prompt: Option<String>) -> Self {
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            system_prompt,
            messages: Vec::new(),
        }
    }
}

/// Versioned provider-neutral model request context
/// (Architecture v2 part 1 §3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// Persisted context schema version.
    pub schema_version: u32,
    /// Optional system instruction.
    pub system_prompt: Option<String>,
    /// Projected model-visible messages.
    pub messages: Vec<Message>,
    /// Tools available for this request.
    pub tools: Vec<ToolSpec>,
}

impl Context {
    /// Creates an empty version-one context.
    pub fn new(system_prompt: Option<String>) -> Self {
        Self {
            schema_version: CONTEXT_SCHEMA_VERSION,
            system_prompt,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// Terminal assistant-message metadata (Architecture v2 part 2 §2.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantFinish {
    /// Normalized terminal reason.
    pub reason: AssistantFinishReason,
    /// Original provider reason when available.
    pub raw_provider_reason: Option<String>,
    /// Sanitized public error for failed or aborted messages.
    pub error: Option<PublicError>,
}

/// Normalized assistant terminal reason (Architecture v2 part 2 §2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantFinishReason {
    /// Model completed normally.
    Stop,
    /// Provider output limit truncated the response.
    Length,
    /// Model requested one or more tools.
    ToolUse,
    /// Provider returned a durable deferred handle.
    Deferred,
    /// Request or stream failed.
    Error,
    /// Caller cancelled the request.
    Aborted,
}

/// Sanitized serializable provider error (Architecture v2 part 2 §2.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicError {
    /// Stable normalized error code.
    pub code: String,
    /// Sanitized display message.
    pub message: String,
    /// Whether the logical operation may be retried.
    pub retryable: bool,
    /// Provider-native error code when safe to expose.
    pub provider_code: Option<String>,
    /// HTTP status when applicable.
    pub status: Option<u16>,
    /// Provider request identifier when safe to expose.
    pub request_id: Option<String>,
}

impl PublicError {
    /// Redacts known credential values and secret-bearing structured fields.
    ///
    /// Provider adapters pass the credential values available at their
    /// boundary. Ordinary status and provider-body diagnostics are retained;
    /// only exact credential values and values assigned to known secret keys
    /// are replaced.
    pub fn sanitized(mut self, secret_values: &[&str]) -> Self {
        self.code = crate::sanitization::redact_public_text(self.code, secret_values);
        self.message = crate::sanitization::redact_public_text(self.message, secret_values);
        self.provider_code = self
            .provider_code
            .map(|value| crate::sanitization::redact_public_text(value, secret_values));
        self.request_id = self
            .request_id
            .map(|value| crate::sanitization::redact_public_text(value, secret_values));
        self
    }
}
