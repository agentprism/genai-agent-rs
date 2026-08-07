//! Version-one request and compact-event DTOs for the proxy wire protocol.
//!
//! These types freeze this crate's JSON boundary independently of `genai`'s own serialization.
//! Only the compact-event side is TypeScript-compatible: event `type` tags remain snake-cased,
//! event fields such as `contentIndex` are camel-cased, and the successful tool terminal reason is
//! exactly `"toolUse"`. The request side is this crate's own schema — [`ProxyRequestV1`] is not
//! the TypeScript `proxy.ts` request body (a pi-ai `Model` object, the pi message schema, and an
//! eleven-option `SimpleStreamOptions` subset), so servers implementing that contract cannot
//! accept these requests without translation. Field names documented in backticks are their exact
//! JSON spellings.
//!
//! The request DTO never contains the proxy bearer token and cannot represent a resolved
//! `ModelSpec::Target`, so a service target's endpoint and credentials do not cross this boundary.
//! It is not otherwise secret-free: `extraHeaders` and `extraBody` are caller-controlled provider
//! data and may themselves contain credentials or other sensitive values.

use crate::StreamRequest;
use genai::adapter::AdapterKind;
use genai::chat::{
    BinarySource, CacheControl, ChatMessage, ChatOptions, ChatResponseFormat, ChatRole,
    ContentPart, MessageOptions, ReasoningEffort, ServiceTier, Tool, ToolChoice, ToolConfig,
    ToolName, Verbosity,
};
use genai::{ModelIden, ModelSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A request cannot be represented by the stable proxy wire contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyRequestError {
    /// The request selected `ModelSpec::Target`.
    ///
    /// Resolved service targets can carry endpoint and authentication material, so callers must
    /// select a model by name or explicit model identity instead.
    #[error("ModelSpec::Target cannot be sent through the proxy")]
    TargetModelUnsupported,

    /// A floating-point option was NaN or infinite and therefore has no JSON representation.
    #[error("proxy option {field} must be finite")]
    NonFiniteOption {
        /// Exact wire field rejected by validation (`"temperature"` or `"topP"`).
        field: &'static str,
    },
}

/// Version-one proxy POST body and stable protocol boundary.
///
/// This is this crate's own request schema, not the TypeScript proxy's: the pi `proxy.ts` client
/// posts a pi-ai `Model` object, pi-schema messages, and an eleven-option `SimpleStreamOptions`
/// subset, so a server implementing that contract cannot accept this body without translation.
///
/// Transport authentication is supplied separately by [`ProxyStreamOptions`](super::ProxyStreamOptions)
/// and is never serialized here. Conversion rejects `ModelSpec::Target` rather than serializing a
/// resolved service endpoint or its credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyRequestV1 {
    /// Model selector serialized as the `"model"` field.
    pub model: ProxyModelV1,
    /// System prompt, messages, and tools serialized as the `"context"` field.
    pub context: ProxyContextV1,
    /// Provider chat options serialized as the `"options"` field.
    pub options: ProxyChatOptionsV1,
}

/// Proxy-safe model selection serialized with a `"type"` discriminator.
///
/// A resolved service target is deliberately not representable; converting `ModelSpec::Target`
/// returns [`ProxyRequestError::TargetModelUnsupported`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyModelV1 {
    /// Resolve a model name on the proxy server (`"type": "name"`).
    #[serde(rename = "name")]
    Name {
        /// Model name serialized as `"name"`.
        name: String,
    },
    /// Select an explicit adapter/model pair (`"type": "identity"`).
    #[serde(rename = "identity")]
    Identity {
        /// Stable adapter namespace serialized as `"adapter"`.
        adapter: String,
        /// Provider model name serialized as `"model"`.
        model: String,
    },
}

/// Explicit model identity used inside provider-specific custom content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyModelIdentityV1 {
    /// Stable adapter namespace serialized as `"adapter"`.
    pub adapter: String,
    /// Provider model name serialized as `"model"`.
    pub model: String,
}

/// Provider context sent to the proxy server, using camel-case JSON field names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyContextV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional system instructions serialized as `"systemPrompt"`; absent values are omitted.
    pub system_prompt: Option<String>,
    /// Ordered conversation messages serialized as `"messages"`.
    pub messages: Vec<ProxyMessageV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Available tools serialized as `"tools"`; absent values are omitted.
    pub tools: Option<Vec<ProxyToolV1>>,
}

/// Stable provider chat-option subset, using camel-case JSON field names.
///
/// Local capture settings and the invocation cancellation token are intentionally absent. The
/// `extraHeaders` and `extraBody` fields remain arbitrary caller data and may contain secrets even
/// though proxy transport and resolved service-target credentials are excluded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyChatOptionsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Sampling temperature serialized as `"temperature"`; conversion rejects non-finite values.
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Maximum generated-token count serialized as `"maxTokens"`.
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Nucleus-sampling probability serialized as `"topP"`; conversion rejects non-finite values.
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Provider stop strings serialized as `"stopSequences"`; an empty list is omitted.
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Structured-response request serialized as `"responseFormat"`.
    pub response_format: Option<ProxyResponseFormatV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Tool-selection policy serialized as `"toolChoice"`.
    pub tool_choice: Option<ProxyToolChoiceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Reasoning-content normalization preference serialized as `"normalizeReasoningContent"`.
    pub normalize_reasoning_content: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Requested reasoning effort serialized as `"reasoningEffort"`.
    pub reasoning_effort: Option<ProxyReasoningEffortV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Requested response verbosity serialized as `"verbosity"`.
    pub verbosity: Option<ProxyVerbosityV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Provider sampling seed serialized as `"seed"`.
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Requested provider service tier serialized as `"serviceTier"`.
    pub service_tier: Option<ProxyServiceTierV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Caller-supplied provider headers serialized as `"extraHeaders"`.
    ///
    /// These are request payload data, not the proxy transport's HTTP headers. They are forwarded
    /// deliberately and may contain application credentials or other sensitive values.
    pub extra_headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Provider cache policy serialized as `"cacheControl"`.
    pub cache_control: Option<ProxyCacheControlV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Provider prompt-cache key serialized as `"promptCacheKey"`.
    pub prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Caller-supplied provider JSON serialized as `"extraBody"`.
    ///
    /// This arbitrary payload is forwarded deliberately and may contain application credentials or
    /// other sensitive values.
    pub extra_body: Option<Value>,
}

/// One conversation message in the version-one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyMessageV1 {
    /// Message author serialized as `"role"`.
    pub role: ProxyMessageRoleV1,
    /// Ordered multimodal parts serialized as `"content"`.
    pub content: Vec<ProxyContentPartV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Per-message provider options serialized as `"options"`; absent values are omitted.
    pub options: Option<ProxyMessageOptionsV1>,
}

/// Message author serialized as one of the lowercase JSON strings below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMessageRoleV1 {
    /// System-authored message (`"system"`).
    System,
    /// User-authored message (`"user"`).
    User,
    /// Assistant-authored message (`"assistant"`).
    Assistant,
    /// Tool-authored response message (`"tool"`).
    Tool,
}

/// Per-message provider options, using camel-case JSON field names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyMessageOptionsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Cache policy serialized as `"cacheControl"`; absent values are omitted.
    pub cache_control: Option<ProxyCacheControlV1>,
}

/// One message content part, serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyContentPartV1 {
    /// Plain text content (`"type": "text"`).
    #[serde(rename = "text")]
    Text {
        /// Text payload serialized as `"text"`.
        text: String,
    },
    /// URL- or base64-backed binary content (`"type": "binary"`).
    #[serde(rename = "binary")]
    Binary {
        #[serde(rename = "contentType")]
        /// Media type serialized as `"contentType"`.
        content_type: String,
        /// Binary location or inline bytes serialized as `"source"`.
        source: ProxyBinarySourceV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Optional display/file name serialized as `"name"`.
        name: Option<String>,
    },
    /// Assistant tool invocation (`"type": "toolCall"`).
    #[serde(rename = "toolCall")]
    ToolCall {
        /// Provider call identifier serialized as `"id"`.
        id: String,
        /// Tool function name serialized as `"name"`.
        name: String,
        /// Complete JSON arguments serialized as `"arguments"`.
        arguments: Value,
        #[serde(
            rename = "thoughtSignatures",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        /// Provider thought signatures serialized as `"thoughtSignatures"`; an empty list is omitted.
        thought_signatures: Vec<String>,
    },
    /// Tool result returned to the model (`"type": "toolResponse"`).
    #[serde(rename = "toolResponse")]
    ToolResponse {
        /// Identifier of the answered tool call, serialized as `"id"`.
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Optional tool function name serialized as `"name"`.
        name: Option<String>,
        /// Textual tool result serialized as `"content"`.
        content: String,
    },
    /// Standalone provider thought signature (`"type": "thoughtSignature"`).
    #[serde(rename = "thoughtSignature")]
    ThoughtSignature {
        /// Opaque signature serialized as `"signature"`.
        signature: String,
    },
    /// Provider reasoning content (`"type": "reasoning"`).
    #[serde(rename = "reasoning")]
    Reasoning {
        /// Reasoning text serialized as `"reasoning"`.
        reasoning: String,
    },
    /// Provider-specific JSON content (`"type": "custom"`).
    #[serde(rename = "custom")]
    Custom {
        /// Provider-specific JSON serialized as `"data"`.
        data: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Optional originating adapter/model identity serialized as `"model"`.
        model: Option<ProxyModelIdentityV1>,
    },
}

/// Location of binary content, serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyBinarySourceV1 {
    /// Remotely hosted binary content (`"type": "url"`).
    #[serde(rename = "url")]
    Url {
        /// Source URL serialized as `"url"`.
        url: String,
    },
    /// Inline base64-encoded binary content (`"type": "base64"`).
    #[serde(rename = "base64")]
    Base64 {
        /// Base64 text serialized as `"data"`.
        data: String,
    },
}

/// Tool declaration in the version-one request, using camel-case JSON field names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyToolV1 {
    /// Custom or built-in tool identity serialized as `"name"`.
    pub name: ProxyToolNameV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Human-readable tool description serialized as `"description"`.
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// JSON input schema serialized as `"schema"`.
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Provider-specific tool format serialized as `"customFormat"`.
    pub custom_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Strict schema-enforcement preference serialized as `"strict"`.
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Provider-specific or web-search configuration serialized as `"config"`.
    pub config: Option<ProxyToolConfigV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Tool-definition cache policy serialized as `"cacheControl"`.
    pub cache_control: Option<ProxyCacheControlV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Eager argument-streaming preference serialized as `"eagerInputStreaming"`.
    pub eager_input_streaming: Option<bool>,
}

/// Tool identity serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyToolNameV1 {
    /// Caller-defined function (`"type": "custom"`).
    #[serde(rename = "custom")]
    Custom {
        /// Function name serialized as `"name"`.
        name: String,
    },
    /// Provider web-search tool (`"type": "webSearch"`).
    #[serde(rename = "webSearch")]
    WebSearch,
}

/// Tool-specific configuration serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyToolConfigV1 {
    /// Arbitrary provider configuration (`"type": "custom"`).
    #[serde(rename = "custom")]
    Custom {
        /// Provider-specific JSON serialized as `"value"`.
        value: Value,
    },
    /// Web-search constraints (`"type": "webSearch"`).
    #[serde(rename = "webSearch")]
    WebSearch {
        #[serde(rename = "maxUses", default, skip_serializing_if = "Option::is_none")]
        /// Optional search-use cap serialized as `"maxUses"`.
        max_uses: Option<u32>,
        #[serde(
            rename = "allowedDomains",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional allowlist serialized as `"allowedDomains"`.
        allowed_domains: Option<Vec<String>>,
        #[serde(
            rename = "blockedDomains",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional denylist serialized as `"blockedDomains"`.
        blocked_domains: Option<Vec<String>>,
    },
}

/// Structured-response mode serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyResponseFormatV1 {
    /// Request unconstrained JSON output (`"type": "jsonMode"`).
    #[serde(rename = "jsonMode")]
    JsonMode,
    /// Request output matching a named JSON schema (`"type": "jsonSpec"`).
    #[serde(rename = "jsonSpec")]
    JsonSpec {
        /// Schema name serialized as `"name"`.
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Optional schema description serialized as `"description"`.
        description: Option<String>,
        /// JSON Schema document serialized as `"schema"`.
        schema: Value,
    },
}

/// Provider tool-selection policy serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyToolChoiceV1 {
    /// Let the provider decide whether to call a tool (`"type": "auto"`).
    #[serde(rename = "auto")]
    Auto,
    /// Prevent tool calls (`"type": "none"`).
    #[serde(rename = "none")]
    None,
    /// Require some tool call (`"type": "required"`).
    #[serde(rename = "required")]
    Required,
    /// Require one named tool (`"type": "tool"`).
    #[serde(rename = "tool")]
    Tool {
        /// Required function name serialized as `"name"`.
        name: String,
    },
}

/// Requested reasoning effort serialized with a `"type"` discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyReasoningEffortV1 {
    /// Disable reasoning effort (`"type": "zero"`).
    #[serde(rename = "zero")]
    Zero,
    /// Request low reasoning effort (`"type": "low"`).
    #[serde(rename = "low")]
    Low,
    /// Request medium reasoning effort (`"type": "medium"`).
    #[serde(rename = "medium")]
    Medium,
    /// Request high reasoning effort (`"type": "high"`).
    #[serde(rename = "high")]
    High,
    /// Request extra-high reasoning effort (`"type": "xhigh"`).
    #[serde(rename = "xhigh")]
    XHigh,
    /// Request the provider's maximum reasoning effort (`"type": "max"`).
    #[serde(rename = "max")]
    Max,
    /// Set an explicit reasoning-token budget (`"type": "budget"`).
    #[serde(rename = "budget")]
    Budget {
        /// Token budget serialized as `"tokens"`.
        tokens: u32,
    },
    /// Request minimal reasoning effort (`"type": "minimal"`).
    #[serde(rename = "minimal")]
    Minimal,
}

/// Requested response verbosity, serialized as a lowercase JSON string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyVerbosityV1 {
    /// Low verbosity (`"low"`).
    Low,
    /// Medium verbosity (`"medium"`).
    Medium,
    /// High verbosity (`"high"`).
    High,
}

/// Requested provider service tier, serialized as a lowercase JSON string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyServiceTierV1 {
    /// Flexible-capacity tier (`"flex"`).
    Flex,
    /// Provider-selected tier (`"auto"`).
    Auto,
    /// Provider default tier (`"default"`).
    Default,
}

/// Provider cache policy serialized as one of the exact JSON strings below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyCacheControlV1 {
    /// Provider-default ephemeral caching (`"ephemeral"`).
    #[serde(rename = "ephemeral")]
    Ephemeral,
    /// Provider memory caching (`"memory"`).
    #[serde(rename = "memory")]
    Memory,
    /// Five-minute ephemeral caching (`"ephemeral5m"`).
    #[serde(rename = "ephemeral5m")]
    Ephemeral5m,
    /// One-hour ephemeral caching (`"ephemeral1h"`).
    #[serde(rename = "ephemeral1h")]
    Ephemeral1h,
    /// Twenty-four-hour ephemeral caching (`"ephemeral24h"`).
    #[serde(rename = "ephemeral24h")]
    Ephemeral24h,
}

/// Compact proxy event carried in a nonempty SSE `data` field.
///
/// Partial assistant snapshots are reconstructed locally and never appear in this DTO. Events must
/// begin with one `start`, use dense zero-based `contentIndex` values, respect each block's
/// start/delta/end lifecycle, and finish with exactly one `done` or `error`. Whitespace-only SSE
/// data fields are keepalive/no-op frames and are ignored before deserialization.
///
/// Progressive tool arguments are parsed with a maximum JSON container nesting depth of 128;
/// deeper snapshots produce the parser's safe empty-object fallback. Each tool call accepts at most
/// 1 MiB of cumulative raw argument JSON and 4,096 deltas (including empty deltas), while all tool
/// calls in one invocation share a 16 MiB cumulative reparse-work limit. A byte, delta-count,
/// reparse-work, or protocol violation produces one partial-preserving in-band terminal error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProxyAssistantMessageEvent {
    /// Begin an assistant stream (`"type": "start"`); valid exactly once and before other events.
    #[serde(rename = "start")]
    Start,
    /// Open a text block (`"type": "text_start"`) at the next dense content slot.
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        /// Zero-based destination slot serialized as `"contentIndex"`.
        content_index: u32,
    },
    /// Append text to an open text block (`"type": "text_delta"`).
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        /// Open text slot serialized as `"contentIndex"`.
        content_index: u32,
        /// Text fragment serialized as `"delta"`.
        delta: String,
    },
    /// Close an open text block (`"type": "text_end"`).
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        /// Open text slot serialized as `"contentIndex"`.
        content_index: u32,
        #[serde(
            rename = "contentSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional opaque provider signature serialized as `"contentSignature"`.
        content_signature: Option<String>,
    },
    /// Open a reasoning block (`"type": "thinking_start"`) at the next dense content slot.
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        /// Zero-based destination slot serialized as `"contentIndex"`.
        content_index: u32,
    },
    /// Append reasoning text to an open reasoning block (`"type": "thinking_delta"`).
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        /// Open reasoning slot serialized as `"contentIndex"`.
        content_index: u32,
        /// Reasoning fragment serialized as `"delta"`.
        delta: String,
    },
    /// Close an open reasoning block (`"type": "thinking_end"`).
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        /// Open reasoning slot serialized as `"contentIndex"`.
        content_index: u32,
        #[serde(
            rename = "contentSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional opaque provider signature serialized as `"contentSignature"`.
        content_signature: Option<String>,
    },
    /// Open a tool-call block (`"type": "toolcall_start"`) at the next dense content slot.
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        /// Zero-based destination slot serialized as `"contentIndex"`.
        content_index: u32,
        /// Provider tool-call identifier serialized as `"id"`.
        id: String,
        #[serde(rename = "toolName")]
        /// Function name serialized as `"toolName"`.
        tool_name: String,
    },
    /// Append raw JSON to an open tool call (`"type": "toolcall_delta"`).
    ///
    /// The cumulative parser limits documented on [`ProxyAssistantMessageEvent`] apply before a
    /// fragment is accepted into the partial call.
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        /// Open tool-call slot serialized as `"contentIndex"`.
        content_index: u32,
        /// Progressive raw JSON fragment serialized as `"delta"`.
        delta: String,
    },
    /// Close an open tool-call block (`"type": "toolcall_end"`).
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        /// Open tool-call slot serialized as `"contentIndex"`.
        content_index: u32,
        #[serde(
            rename = "thoughtSignatures",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        /// Provider thought signatures serialized as `"thoughtSignatures"`; an empty list is omitted.
        thought_signatures: Vec<String>,
    },
    /// Successfully terminate the stream (`"type": "done"`).
    ///
    /// All content blocks must already be closed.
    #[serde(rename = "done")]
    Done {
        /// Successful stop reason serialized as `"reason"`.
        reason: ProxyDoneReason,
        /// Final provider usage serialized as `"usage"`.
        usage: ProxyUsage,
        #[serde(
            rename = "responseId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional provider response identifier serialized as `"responseId"`.
        response_id: Option<String>,
        #[serde(
            rename = "providerStopReason",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional raw provider stop reason serialized as `"providerStopReason"`.
        provider_stop_reason: Option<String>,
    },
    /// Terminate with a server-reported failure or cancellation (`"type": "error"`).
    ///
    /// Accumulated content remains present in the emitted terminal assistant message.
    #[serde(rename = "error")]
    Error {
        /// Failure class serialized as `"reason"`.
        reason: ProxyErrorReason,
        #[serde(
            rename = "errorMessage",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional human-readable diagnostic serialized as `"errorMessage"`.
        error_message: Option<String>,
        /// Final provider usage serialized as `"usage"`.
        usage: ProxyUsage,
        #[serde(
            rename = "responseId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional provider response identifier serialized as `"responseId"`.
        response_id: Option<String>,
        #[serde(
            rename = "providerStopReason",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        /// Optional raw provider stop reason serialized as `"providerStopReason"`.
        provider_stop_reason: Option<String>,
    },
}

/// Successful terminal reason serialized as one of the exact JSON strings below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyDoneReason {
    /// The provider completed normally (`"stop"`).
    #[serde(rename = "stop")]
    Stop,
    /// The provider reached a generation limit (`"length"`).
    #[serde(rename = "length")]
    Length,
    /// The provider requested tool execution (`"toolUse"`, with this exact camel-case spelling).
    #[serde(rename = "toolUse")]
    ToolUse,
}

/// Unsuccessful terminal reason serialized as one of the exact JSON strings below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyErrorReason {
    /// The proxy or provider failed (`"error"`).
    #[serde(rename = "error")]
    Error,
    /// The invocation was cancelled (`"aborted"`).
    #[serde(rename = "aborted")]
    Aborted,
}

/// Provider token usage in a terminal event, using camel-case JSON field names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyUsage {
    /// Input-token count serialized as `"input"`.
    pub input: u64,
    /// Output-token count serialized as `"output"`.
    pub output: u64,
    /// Tokens read from a provider cache, serialized as `"cacheRead"`.
    pub cache_read: u64,
    /// Tokens written to a provider cache, serialized as `"cacheWrite"`.
    pub cache_write: u64,
    /// Provider-reported total-token count serialized as `"totalTokens"`.
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional provider-reported monetary cost serialized as `"cost"`.
    ///
    /// The proxy assistant accumulator currently maps token counts into `AgentUsage`; monetary cost
    /// remains wire metadata and is not represented by that local usage type.
    pub cost: Option<ProxyUsageCost>,
}

/// Provider-reported monetary usage components, using camel-case JSON field names.
///
/// Units and currency are defined by the proxy/provider contract; this DTO does not reinterpret
/// them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyUsageCost {
    /// Input cost serialized as `"input"`.
    pub input: f64,
    /// Output cost serialized as `"output"`.
    pub output: f64,
    /// Cache-read cost serialized as `"cacheRead"`.
    pub cache_read: f64,
    /// Cache-write cost serialized as `"cacheWrite"`.
    pub cache_write: f64,
    /// Total cost serialized as `"total"`.
    pub total: f64,
}

impl TryFrom<&StreamRequest> for ProxyRequestV1 {
    type Error = ProxyRequestError;

    fn try_from(request: &StreamRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            model: ProxyModelV1::try_from(&request.model)?,
            context: ProxyContextV1::from(&request.context),
            options: ProxyChatOptionsV1::try_from(&request.options)?,
        })
    }
}

impl TryFrom<StreamRequest> for ProxyRequestV1 {
    type Error = ProxyRequestError;

    fn try_from(request: StreamRequest) -> Result<Self, Self::Error> {
        Self::try_from(&request)
    }
}

impl TryFrom<&ModelSpec> for ProxyModelV1 {
    type Error = ProxyRequestError;

    fn try_from(model: &ModelSpec) -> Result<Self, Self::Error> {
        match model {
            ModelSpec::Name(name) => Ok(Self::Name {
                name: name.to_string(),
            }),
            ModelSpec::Iden(model) => Ok(Self::Identity {
                adapter: adapter_namespace(model.adapter_kind),
                model: model.model_name.to_string(),
            }),
            ModelSpec::Target(_) => Err(ProxyRequestError::TargetModelUnsupported),
        }
    }
}

impl From<&crate::LlmContext> for ProxyContextV1 {
    fn from(context: &crate::LlmContext) -> Self {
        Self {
            system_prompt: (!context.system_prompt.is_empty())
                .then(|| context.system_prompt.clone()),
            messages: context.messages.iter().map(ProxyMessageV1::from).collect(),
            tools: (!context.tools.is_empty())
                .then(|| context.tools.iter().map(ProxyToolV1::from).collect()),
        }
    }
}

impl TryFrom<&ChatOptions> for ProxyChatOptionsV1 {
    type Error = ProxyRequestError;

    fn try_from(options: &ChatOptions) -> Result<Self, Self::Error> {
        if options.temperature.is_some_and(|value| !value.is_finite()) {
            return Err(ProxyRequestError::NonFiniteOption {
                field: "temperature",
            });
        }
        if options.top_p.is_some_and(|value| !value.is_finite()) {
            return Err(ProxyRequestError::NonFiniteOption { field: "topP" });
        }
        Ok(Self {
            temperature: options.temperature,
            max_tokens: options.max_tokens,
            top_p: options.top_p,
            stop_sequences: options.stop_sequences.clone(),
            response_format: options
                .response_format
                .as_ref()
                .map(ProxyResponseFormatV1::from),
            tool_choice: options.tool_choice.as_ref().map(ProxyToolChoiceV1::from),
            normalize_reasoning_content: options.normalize_reasoning_content,
            reasoning_effort: options
                .reasoning_effort
                .as_ref()
                .map(ProxyReasoningEffortV1::from),
            verbosity: options.verbosity.as_ref().map(ProxyVerbosityV1::from),
            seed: options.seed,
            service_tier: options.service_tier.as_ref().map(ProxyServiceTierV1::from),
            extra_headers: options.extra_headers.as_ref().map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            }),
            cache_control: options
                .cache_control
                .as_ref()
                .map(ProxyCacheControlV1::from),
            prompt_cache_key: options.prompt_cache_key.clone(),
            extra_body: options.extra_body.clone(),
        })
    }
}

impl From<&ChatMessage> for ProxyMessageV1 {
    fn from(message: &ChatMessage) -> Self {
        Self {
            role: ProxyMessageRoleV1::from(&message.role),
            content: message
                .content
                .iter()
                .map(ProxyContentPartV1::from)
                .collect(),
            options: message.options.as_ref().and_then(proxy_message_options),
        }
    }
}

fn proxy_message_options(options: &MessageOptions) -> Option<ProxyMessageOptionsV1> {
    let options = ProxyMessageOptionsV1 {
        cache_control: options
            .cache_control
            .as_ref()
            .map(ProxyCacheControlV1::from),
    };
    options.cache_control.is_some().then_some(options)
}

impl From<&ChatRole> for ProxyMessageRoleV1 {
    fn from(role: &ChatRole) -> Self {
        match role {
            ChatRole::System => Self::System,
            ChatRole::User => Self::User,
            ChatRole::Assistant => Self::Assistant,
            ChatRole::Tool => Self::Tool,
        }
    }
}

impl From<&ContentPart> for ProxyContentPartV1 {
    fn from(part: &ContentPart) -> Self {
        match part {
            ContentPart::Text(text) => Self::Text { text: text.clone() },
            ContentPart::Binary(binary) => Self::Binary {
                content_type: binary.content_type.clone(),
                source: match &binary.source {
                    BinarySource::Url(url) => ProxyBinarySourceV1::Url { url: url.clone() },
                    BinarySource::Base64(data) => ProxyBinarySourceV1::Base64 {
                        data: data.to_string(),
                    },
                },
                name: binary.name.clone(),
            },
            ContentPart::ToolCall(call) => Self::ToolCall {
                id: call.call_id.clone(),
                name: call.fn_name.clone(),
                arguments: call.fn_arguments.clone(),
                thought_signatures: call.thought_signatures.clone().unwrap_or_default(),
            },
            ContentPart::ToolResponse(response) => Self::ToolResponse {
                id: response.call_id.clone(),
                name: response.fn_name.clone(),
                content: response.content.clone(),
            },
            ContentPart::ThoughtSignature(signature) => Self::ThoughtSignature {
                signature: signature.clone(),
            },
            ContentPart::ReasoningContent(reasoning) => Self::Reasoning {
                reasoning: reasoning.clone(),
            },
            ContentPart::Custom(custom) => Self::Custom {
                data: custom.data.clone(),
                model: custom.model_iden.as_ref().map(ProxyModelIdentityV1::from),
            },
        }
    }
}

impl From<&ModelIden> for ProxyModelIdentityV1 {
    fn from(model: &ModelIden) -> Self {
        Self {
            adapter: adapter_namespace(model.adapter_kind),
            model: model.model_name.to_string(),
        }
    }
}

impl From<&Tool> for ProxyToolV1 {
    fn from(tool: &Tool) -> Self {
        Self {
            name: ProxyToolNameV1::from(&tool.name),
            description: tool.description.clone(),
            schema: tool.schema.clone(),
            custom_format: tool.custom_format.clone(),
            strict: tool.strict,
            config: tool.config.as_ref().map(ProxyToolConfigV1::from),
            cache_control: tool.cache_control.as_ref().map(ProxyCacheControlV1::from),
            eager_input_streaming: tool.eager_input_streaming,
        }
    }
}

impl From<&ToolName> for ProxyToolNameV1 {
    fn from(name: &ToolName) -> Self {
        match name {
            ToolName::Custom(name) => Self::Custom { name: name.clone() },
            ToolName::WebSearch => Self::WebSearch,
        }
    }
}

impl From<&ToolConfig> for ProxyToolConfigV1 {
    fn from(config: &ToolConfig) -> Self {
        match config {
            ToolConfig::Custom(value) => Self::Custom {
                value: value.clone(),
            },
            ToolConfig::WebSearch(config) => Self::WebSearch {
                max_uses: config.max_uses,
                allowed_domains: config.allowed_domains.clone(),
                blocked_domains: config.blocked_domains.clone(),
            },
        }
    }
}

impl From<&ChatResponseFormat> for ProxyResponseFormatV1 {
    fn from(format: &ChatResponseFormat) -> Self {
        match format {
            ChatResponseFormat::JsonMode => Self::JsonMode,
            ChatResponseFormat::JsonSpec(spec) => Self::JsonSpec {
                name: spec.name.clone(),
                description: spec.description.clone(),
                schema: spec.schema.clone(),
            },
        }
    }
}

impl From<&ToolChoice> for ProxyToolChoiceV1 {
    fn from(choice: &ToolChoice) -> Self {
        match choice {
            ToolChoice::Auto => Self::Auto,
            ToolChoice::None => Self::None,
            ToolChoice::Required => Self::Required,
            ToolChoice::Tool { name } => Self::Tool { name: name.clone() },
        }
    }
}

impl From<&ReasoningEffort> for ProxyReasoningEffortV1 {
    fn from(effort: &ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Zero => Self::Zero,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::XHigh => Self::XHigh,
            ReasoningEffort::Max => Self::Max,
            ReasoningEffort::Budget(tokens) => Self::Budget { tokens: *tokens },
            ReasoningEffort::Minimal => Self::Minimal,
        }
    }
}

impl From<&Verbosity> for ProxyVerbosityV1 {
    fn from(verbosity: &Verbosity) -> Self {
        match verbosity {
            Verbosity::Low => Self::Low,
            Verbosity::Medium => Self::Medium,
            Verbosity::High => Self::High,
        }
    }
}

impl From<&ServiceTier> for ProxyServiceTierV1 {
    fn from(tier: &ServiceTier) -> Self {
        match tier {
            ServiceTier::Flex => Self::Flex,
            ServiceTier::Auto => Self::Auto,
            ServiceTier::Default => Self::Default,
        }
    }
}

impl From<&CacheControl> for ProxyCacheControlV1 {
    fn from(control: &CacheControl) -> Self {
        match control {
            CacheControl::Ephemeral => Self::Ephemeral,
            CacheControl::Memory => Self::Memory,
            CacheControl::Ephemeral5m => Self::Ephemeral5m,
            CacheControl::Ephemeral1h => Self::Ephemeral1h,
            CacheControl::Ephemeral24h => Self::Ephemeral24h,
        }
    }
}

fn adapter_namespace(adapter: AdapterKind) -> String {
    let display = adapter.to_string();
    match display.as_str() {
        "OpenAI" => "openai".to_owned(),
        "OpenAIResp" => "openai_resp".to_owned(),
        "Gemini" => "gemini".to_owned(),
        "Anthropic" => "anthropic".to_owned(),
        "Fireworks" => "fireworks".to_owned(),
        "Together" => "together".to_owned(),
        "Groq" => "groq".to_owned(),
        "Aihubmix" => "aihubmix".to_owned(),
        "Kimi" => "kimi".to_owned(),
        "Mimo" => "mimo".to_owned(),
        "Moonshot" => "moonshot".to_owned(),
        "Nebius" => "nebius".to_owned(),
        "Xai" => "xai".to_owned(),
        "DeepSeek" => "deepseek".to_owned(),
        "Zai" => "zai".to_owned(),
        "BigModel" => "bigmodel".to_owned(),
        "Aliyun" => "aliyun".to_owned(),
        "QwenCloud" => "qwen_cloud".to_owned(),
        "Baidu" => "baidu".to_owned(),
        "Cohere" => "cohere".to_owned(),
        "Ollama" => "ollama".to_owned(),
        "OllamaCloud" => "ollama_cloud".to_owned(),
        "Omlx" => "omlx".to_owned(),
        "Vertex" => "vertex".to_owned(),
        "GithubCopilot" => "github_copilot".to_owned(),
        "OpenCodeGo" => "opencode_go".to_owned(),
        "BedrockApi" => "bedrock_api".to_owned(),
        "BedrockSigv4" => "bedrock_sigv4".to_owned(),
        "OpenRouter" => "open_router".to_owned(),
        "AtlasCloud" => "atlascloud".to_owned(),
        "MiniMax" => "minimax".to_owned(),
        custom if custom.starts_with("genai_") => custom.to_owned(),
        other => other.to_ascii_lowercase(),
    }
}
