//! Typed model descriptors from Architecture v2 part 2 §5.1–§5.2.

use crate::{ApiId, ExtensionId, ModelId, ModelPricing, ModelRef, OrderedJsonObject, ProviderId};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Number, value::RawValue};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use url::Url;

/// Insertion-ordered chat-template values used by OpenAI-compatible model
/// compatibility data (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
pub type ChatTemplateValues = IndexMap<String, ChatTemplateKwargValue>;

/// Case-preserving logical headers with `None` as an explicit deletion marker
/// (Architecture v2 part 2 §5.1 and §2.5).
pub type HeaderMapSpec = BTreeMap<String, Option<String>>;

/// Namespaced extensions not consumed by core lowering
/// (Architecture v2 part 2 §5.1).
///
/// API-family lowering reads only typed config; provider middleware reads only
/// declared namespaces; unknown entries survive persistence; and core behavior
/// must never depend on an ad hoc key in this map.
pub type ExtensionMap = BTreeMap<ExtensionId, VersionedExtension>;

/// Complete typed model descriptor (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Fields shared by all API families.
    pub common: CommonModelDescriptor,
    /// Typed lowering configuration for one API family.
    pub api: ApiModelConfig,
    /// Namespaced data not consumed by core lowering.
    pub extensions: ExtensionMap,
}

/// API-independent model catalog fields (Architecture v2 part 2 §5.1).
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommonModelDescriptor {
    /// Provider/model identity.
    pub model_ref: ModelRef,
    /// Human-readable display name.
    pub display_name: String,
    /// Default provider endpoint.
    pub base_url: Url,
    /// Supported input and output modalities.
    pub modalities: ModalityCapabilities,
    /// Context and output limits.
    pub limits: ModelLimits,
    /// Integer token pricing.
    pub pricing: ModelPricing,
    /// Whether the model supports reasoning controls.
    pub reasoning: bool,
    /// Per-model logical headers.
    pub headers: HeaderMapSpec,
}

impl fmt::Debug for CommonModelDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommonModelDescriptor")
            .field("model_ref", &self.model_ref)
            .field("display_name", &self.display_name)
            .field("base_url", &"<redacted endpoint>")
            .field("modalities", &self.modalities)
            .field("limits", &self.limits)
            .field("pricing", &self.pricing)
            .field("reasoning", &self.reasoning)
            .field("headers", &"<redacted headers>")
            .finish()
    }
}

/// A model input or output modality (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// UTF-8 text.
    Text,
    /// Raster or encoded image content.
    Image,
    /// Audio content.
    Audio,
}

/// Explicit input/output modality sets (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModalityCapabilities {
    /// Modalities accepted in requests.
    pub input: BTreeSet<Modality>,
    /// Modalities the model can produce.
    pub output: BTreeSet<Modality>,
}

/// Model token limits (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Total provider context window.
    pub context_window: u64,
    /// Maximum output tokens accepted by the provider.
    pub max_output_tokens: u32,
}

/// Typed API-family model configuration (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "api", content = "config")]
#[allow(
    clippy::large_enum_variant,
    reason = "Architecture v2 part 2 §5.1 specifies direct typed API config variants"
)]
pub enum ApiModelConfig {
    /// OpenAI-compatible Chat Completions.
    #[serde(rename = "openai-completions")]
    OpenAiCompletions(OpenAiCompletionsModelConfig),
    /// OpenAI Responses.
    #[serde(rename = "openai-responses")]
    OpenAiResponses(OpenAiResponsesModelConfig),
    /// Anthropic Messages.
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages(AnthropicMessagesModelConfig),
    /// Google Gemini Developer API.
    #[serde(rename = "google-generative-ai")]
    GoogleGenerativeAi(GoogleModelConfig),
    /// Google Vertex Gemini API.
    #[serde(rename = "google-vertex")]
    GoogleVertex(GoogleModelConfig),
    /// Amazon Bedrock Converse Stream.
    #[serde(rename = "bedrock-converse-stream")]
    BedrockConverse(BedrockModelConfig),
    /// Mistral Conversations.
    #[serde(rename = "mistral-conversations")]
    MistralConversations(MistralModelConfig),
    /// Third-party API family with a versioned typed payload.
    #[serde(rename = "custom")]
    Custom(CustomApiModelConfig),
}

impl ApiModelConfig {
    /// Returns the open API-family identifier represented by this config.
    pub fn api_id(&self) -> ApiId {
        match self {
            Self::OpenAiCompletions(_) => ApiId::new("openai-completions"),
            Self::OpenAiResponses(_) => ApiId::new("openai-responses"),
            Self::AnthropicMessages(_) => ApiId::new("anthropic-messages"),
            Self::GoogleGenerativeAi(_) => ApiId::new("google-generative-ai"),
            Self::GoogleVertex(_) => ApiId::new("google-vertex"),
            Self::BedrockConverse(_) => ApiId::new("bedrock-converse-stream"),
            Self::MistralConversations(_) => ApiId::new("mistral-conversations"),
            Self::Custom(config) => config.api.clone(),
        }
    }
}

/// OpenAI Chat Completions model lowering data
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OpenAiCompletionsModelConfig {
    /// Typed compatibility overrides.
    pub compat: OpenAiCompletionsCompat,
    /// Explicit per-level model mappings.
    pub thinking_levels: ThinkingLevelMap<OpenAiThinkingValue>,
    /// Insertion-ordered sampling defaults.
    pub sampling_defaults: OrderedJsonObject,
}

/// OpenAI Responses model lowering data (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OpenAiResponsesModelConfig {
    /// Typed compatibility overrides.
    pub compat: OpenAiResponsesCompat,
    /// Explicit per-level model mappings.
    pub thinking_levels: ThinkingLevelMap<OpenAiThinkingValue>,
    /// Insertion-ordered sampling defaults.
    pub sampling_defaults: OrderedJsonObject,
}

/// Provider/model-specific OpenAI reasoning value
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum OpenAiThinkingValue {
    /// Explicitly disable reasoning.
    Disabled,
    /// Provider-native effort string.
    Effort(String),
    /// Provider-native reasoning-token budget.
    TokenBudget(u32),
}

/// Typed OpenAI-compatible Chat Completions compatibility overrides
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCompletionsCompat {
    /// Whether the endpoint accepts `store`.
    pub supports_store: Option<bool>,
    /// Whether the endpoint accepts the developer role.
    pub supports_developer_role: Option<bool>,
    /// Whether the endpoint accepts reasoning effort.
    pub supports_reasoning_effort: Option<bool>,
    /// Whether streaming usage can be requested.
    pub supports_usage_in_streaming: Option<bool>,
    /// Whether streamed responses carry a finish reason.
    pub supports_finish_reason: Option<bool>,
    /// Request field used for maximum output tokens.
    pub max_tokens_field: Option<MaxTokensField>,
    /// Whether tool results require a name.
    pub requires_tool_result_name: Option<bool>,
    /// Whether tool results must be followed by an assistant bridge.
    pub requires_assistant_after_tool_result: Option<bool>,
    /// Whether visible thinking must be lowered to plain text.
    pub requires_thinking_as_text: Option<bool>,
    /// Whether replayed assistant messages require `reasoning_content`.
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    /// Provider reasoning request shape.
    pub thinking_format: Option<OpenAiThinkingFormat>,
    /// Values sent as `chat_template_kwargs` for the configurable
    /// chat-template thinking format.
    pub chat_template_kwargs: Option<ChatTemplateValues>,
    /// Values sent as `chat_template_args` for Baseten's thinking format.
    pub chat_template_args: Option<ChatTemplateValues>,
    /// OpenRouter provider-selection and routing preferences.
    pub open_router_routing: Option<OpenRouterRouting>,
    /// Vercel AI Gateway provider routing preferences.
    pub vercel_gateway_routing: Option<VercelGatewayRouting>,
    /// Whether z.ai accepts top-level `tool_stream: true`.
    pub zai_tool_stream: Option<bool>,
    /// Top-level field used for reasoning token budgets.
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    /// Legacy vLLM alias for `thinking_token_budget_field`.
    pub supports_thinking_token_budget: Option<bool>,
    /// Whether strict JSON-schema tools are accepted.
    pub supports_strict_mode: Option<bool>,
    /// Whether OpenAI grammar tools are accepted.
    pub supports_openai_grammar_tools: Option<bool>,
    /// Prompt-cache marker format.
    pub cache_control_format: Option<CacheControlFormat>,
    /// Whether session-affinity values should be sent at all.
    pub send_session_affinity_headers: Option<bool>,
    /// Provider-specific deferred-tool serialization convention.
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    /// Session-affinity header convention.
    pub session_affinity_format: Option<SessionAffinityFormat>,
    /// Whether long prompt-cache retention is accepted.
    pub supports_long_cache_retention: Option<bool>,
    /// Forward-compatible fields scoped to this API family.
    pub extensions: ExtensionMap,
}

/// OpenAI max-output request field (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_completion_tokens`.
    MaxCompletionTokens,
    /// `max_tokens`.
    MaxTokens,
}

/// One value in `chat_template_kwargs` or `chat_template_args`
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    /// Literal UTF-8 string.
    String(String),
    /// Exact JSON number; this avoids narrowing through a floating-point type.
    Number(Number),
    /// Literal boolean.
    Boolean(bool),
    /// Literal JSON null.
    Null,
    /// Value supplied by Pi's thinking planner.
    Variable(ChatTemplateVariable),
}

/// A Pi-controlled value substituted into a chat-template argument
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatTemplateVariable {
    /// Planner value to substitute. Pi spells this property `$var`.
    #[serde(rename = "$var")]
    pub variable: ChatTemplateVariableName,
    /// Omit the surrounding template argument when reasoning is disabled.
    #[serde(
        rename = "omitWhenOff",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub omit_when_off: Option<bool>,
}

/// Pi-controlled chat-template substitution name
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatTemplateVariableName {
    /// Whether thinking is enabled.
    #[serde(rename = "thinking.enabled")]
    ThinkingEnabled,
    /// Provider/model-specific thinking effort.
    #[serde(rename = "thinking.effort")]
    ThinkingEffort,
    /// Provider/model-specific thinking token budget.
    #[serde(rename = "thinking.budget")]
    ThinkingBudget,
}

/// OpenRouter provider routing preferences
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouterRouting {
    /// Whether backup providers may serve the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    /// Whether candidate providers must support every request parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    /// Upstream provider data-collection policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<OpenRouterDataCollection>,
    /// Whether routing is limited to zero-data-retention endpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    /// Whether endpoints must allow text distillation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,
    /// Ordered providers to try.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// Exclusive provider allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Provider denylist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// Accepted quantization names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,
    /// Provider sort rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<OpenRouterSort>,
    /// Maximum accepted prices without floating-point monetary storage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price: Option<OpenRouterMaxPrice>,
    /// Preferred minimum throughput.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_min_throughput: Option<OpenRouterMetricPreference>,
    /// Preferred maximum latency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_max_latency: Option<OpenRouterMetricPreference>,
}

/// OpenRouter data-collection preference
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterDataCollection {
    /// Reject providers that may collect request data.
    Deny,
    /// Allow providers that may store or train on request data.
    Allow,
}

/// OpenRouter provider sorting rule
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenRouterSort {
    /// Shorthand metric name.
    Name(String),
    /// Structured sort and partition configuration.
    Options(OpenRouterSortOptions),
}

/// Structured OpenRouter sorting configuration
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouterSortOptions {
    /// Sorting metric such as `price`, `throughput`, or `latency`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Optional partitioning strategy, including an explicit JSON null.
    #[serde(default, skip_serializing_if = "NullableString::is_absent")]
    pub partition: NullableString,
}

/// A nullable string that preserves the difference between an absent field
/// and an explicit JSON null (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum NullableString {
    /// Field was absent. Struct fields skip this value during serialization.
    #[default]
    Absent,
    /// Explicit JSON null.
    Null,
    /// UTF-8 string value.
    String(String),
}

impl NullableString {
    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl Serialize for NullableString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::String(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for NullableString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::String(value),
            None => Self::Null,
        })
    }
}

/// OpenRouter maximum prices per request unit
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouterMaxPrice {
    /// Maximum price per million prompt tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<JsonNumberOrString>,
    /// Maximum price per million completion tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<JsonNumberOrString>,
    /// Maximum price per image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<JsonNumberOrString>,
    /// Maximum price per audio unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<JsonNumberOrString>,
    /// Maximum price per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<JsonNumberOrString>,
}

/// Exact JSON number-or-string value used by OpenRouter price constraints
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonNumberOrString {
    /// Exact JSON number retained without an `f64` representation.
    Number(Number),
    /// Decimal or provider-specific string representation.
    String(String),
}

/// OpenRouter throughput or latency preference
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenRouterMetricPreference {
    /// Shorthand value applied to the fiftieth percentile.
    Number(Number),
    /// Percentile-specific values.
    Percentiles(OpenRouterPercentiles),
}

/// Percentile-specific OpenRouter throughput or latency cutoffs
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenRouterPercentiles {
    /// Fiftieth-percentile cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<Number>,
    /// Seventy-fifth-percentile cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p75: Option<Number>,
    /// Ninetieth-percentile cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p90: Option<Number>,
    /// Ninety-ninth-percentile cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<Number>,
}

/// Vercel AI Gateway provider routing preferences
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VercelGatewayRouting {
    /// Exclusive provider allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Ordered provider preference list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}

/// OpenAI-compatible reasoning request convention
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiThinkingFormat {
    /// OpenAI `reasoning_effort`.
    #[serde(rename = "openai")]
    OpenAi,
    /// OpenRouter nested `reasoning`.
    #[serde(rename = "openrouter")]
    OpenRouter,
    /// DeepSeek thinking object.
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// Together reasoning object.
    #[serde(rename = "together")]
    Together,
    /// Baseten chat-template arguments.
    #[serde(rename = "baseten")]
    Baseten,
    /// z.ai thinking object.
    #[serde(rename = "zai")]
    Zai,
    /// Qwen top-level switch.
    #[serde(rename = "qwen")]
    Qwen,
    /// Configured chat-template keyword arguments.
    #[serde(rename = "chat-template")]
    ChatTemplate,
    /// Qwen chat-template keyword arguments.
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    /// Top-level string thinking value.
    #[serde(rename = "string-thinking")]
    StringThinking,
    /// Ant Ling nested reasoning effort.
    #[serde(rename = "ant-ling")]
    AntLing,
}

/// Top-level reasoning budget field for OpenAI-compatible APIs
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingTokenBudgetField {
    /// `thinking_token_budget`.
    ThinkingTokenBudget,
    /// `thinking_budget`.
    ThinkingBudget,
    /// `thinking_budget_tokens`.
    ThinkingBudgetTokens,
}

/// Prompt-cache marker convention (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlFormat {
    /// Anthropic-compatible cache-control markers.
    Anthropic,
}

/// Provider-specific deferred-tool serialization mode
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeferredToolsMode {
    /// Kimi deferred-tool serialization.
    #[serde(rename = "kimi")]
    Kimi,
}

/// Session-affinity convention (Architecture v2 part 2 §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionAffinityFormat {
    /// OpenAI session and client headers.
    #[serde(rename = "openai")]
    OpenAi,
    /// OpenAI client headers without `session_id`.
    #[serde(rename = "openai-nosession")]
    OpenAiNoSession,
    /// OpenRouter session header.
    #[serde(rename = "openrouter")]
    OpenRouter,
}

/// Typed OpenAI Responses compatibility overrides
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiResponsesCompat {
    /// Whether developer-role messages are accepted.
    pub supports_developer_role: Option<bool>,
    /// Session-affinity header convention.
    pub session_affinity_format: Option<SessionAffinityFormat>,
    /// Whether long prompt-cache retention is accepted.
    pub supports_long_cache_retention: Option<bool>,
    /// Whether strict function schemas are accepted.
    pub supports_strict_mode: Option<bool>,
    /// Whether OpenAI grammar tools are accepted.
    pub supports_openai_grammar_tools: Option<bool>,
    /// Whether message-anchored additional tools are accepted.
    pub supports_additional_tools: Option<bool>,
    /// Whether client-executed tool search is accepted.
    pub supports_tool_search: Option<bool>,
    /// Whether explicit prompt-cache mode is accepted.
    pub supports_explicit_prompt_cache_mode: Option<bool>,
    /// Forward-compatible fields scoped to this API family.
    pub extensions: ExtensionMap,
}

/// Anthropic Messages model lowering data (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessagesModelConfig {
    /// Typed compatibility overrides.
    pub compat: AnthropicMessagesCompat,
    /// Explicit per-level model mappings.
    pub thinking_levels: ThinkingLevelMap<AnthropicThinkingValue>,
}

/// Provider/model-specific Anthropic reasoning value
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AnthropicThinkingValue {
    /// Explicitly turn reasoning off.
    Off,
    /// Adaptive-thinking effort.
    Effort(AnthropicEffort),
    /// Fixed thinking-token budget.
    Budget(u32),
}

/// Anthropic adaptive-thinking effort (Architecture v2 part 2 §3.5 and §5.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicEffort {
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Extra-high effort.
    Xhigh,
    /// Unconstrained maximum effort.
    Max,
}

/// Typed Anthropic Messages compatibility overrides
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicMessagesCompat {
    /// Whether tools accept eager input streaming.
    pub supports_eager_tool_input_streaming: Option<bool>,
    /// Whether one-hour cache retention is accepted.
    pub supports_long_cache_retention: Option<bool>,
    /// Whether session-affinity headers should be sent.
    pub send_session_affinity_headers: Option<bool>,
    /// Whether cache control may be attached to tools.
    pub supports_cache_control_on_tools: Option<bool>,
    /// Whether the model accepts temperature.
    pub supports_temperature: Option<bool>,
    /// Whether adaptive thinking must be forced.
    pub force_adaptive_thinking: Option<bool>,
    /// Whether an empty thinking signature may be replayed.
    pub allow_empty_signature: Option<bool>,
    /// Whether strict tool schemas are accepted.
    pub supports_strict_tools: Option<bool>,
    /// Whether deferred tool references are accepted.
    pub supports_tool_references: Option<bool>,
    /// Provider-accepted server-side fallback models.
    pub allowed_fallback_models: Vec<AnthropicFallbackModel>,
    /// Forward-compatible fields scoped to this API family.
    pub extensions: ExtensionMap,
}

/// Anthropic server-side fallback target and local pricing
/// (Architecture v2 part 2 §5.1; pinned Pi `types.ts`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnthropicFallbackModel {
    /// Provider expected to return the fallback.
    pub provider: ProviderId,
    /// Fallback model identifier.
    pub model: ModelId,
    /// Local pricing used if the fallback is selected.
    pub cost: ModelPricing,
}

/// Google API-family model lowering data (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleModelConfig {
    /// Explicit per-level provider strings or unsupported markers.
    pub thinking_levels: ThinkingLevelMap<String>,
}

/// Bedrock model lowering data (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BedrockModelConfig {
    /// Bedrock compatibility overrides.
    pub compat: BedrockCompat,
    /// Explicit per-level provider strings or unsupported markers.
    pub thinking_levels: ThinkingLevelMap<String>,
}

/// Typed Bedrock compatibility overrides (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BedrockCompat {
    /// Whether Bedrock strict tool schemas are accepted.
    pub supports_strict_mode: Option<bool>,
    /// Forward-compatible fields scoped to this API family.
    pub extensions: ExtensionMap,
}

/// Mistral Conversations model lowering data (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MistralModelConfig {
    /// Explicit per-level provider strings or unsupported markers.
    pub thinking_levels: ThinkingLevelMap<String>,
}

/// Custom API-family model configuration (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomApiModelConfig {
    /// Open custom API-family identifier.
    pub api: ApiId,
    /// Schema version understood by the custom API crate.
    pub schema_version: u32,
    /// Exact custom configuration JSON.
    pub value: Box<RawValue>,
}

impl PartialEq for CustomApiModelConfig {
    fn eq(&self, other: &Self) -> bool {
        self.api == other.api
            && self.schema_version == other.schema_version
            && self.value.get() == other.value.get()
    }
}

impl Eq for CustomApiModelConfig {}

/// Explicit per-reasoning-level mappings (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: Deserialize<'de>"))]
pub struct ThinkingLevelMap<T> {
    /// Mapping for reasoning disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<LevelSupport<T>>,
    /// Mapping for minimal reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<LevelSupport<T>>,
    /// Mapping for low reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<LevelSupport<T>>,
    /// Mapping for medium reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<LevelSupport<T>>,
    /// Mapping for high reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<LevelSupport<T>>,
    /// Mapping for extra-high reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xhigh: Option<LevelSupport<T>>,
    /// Mapping for maximum reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<LevelSupport<T>>,
}

impl<T> Default for ThinkingLevelMap<T> {
    fn default() -> Self {
        Self {
            off: None,
            minimal: None,
            low: None,
            medium: None,
            high: None,
            xhigh: None,
            max: None,
        }
    }
}

/// Whether a catalog explicitly supports a reasoning level
/// (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "support", content = "value", rename_all = "snake_case")]
pub enum LevelSupport<T> {
    /// Catalog explicitly marks the level unsupported.
    Unsupported,
    /// Catalog explicitly disables reasoning at this level.
    Disabled,
    /// Catalog maps the level to a provider/model-specific value.
    Value(T),
}

/// Resolution of one requested reasoning level against a typed model map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReasoningLevelResolution<T> {
    /// Level supplied by the caller.
    pub requested: crate::ReasoningLevel,
    /// Effective level after Pi's upward-first, then downward clamp search.
    pub effective: crate::ReasoningLevel,
    /// Explicit model support; `None` means use the API-family default map.
    pub support: Option<LevelSupport<T>>,
    /// Whether unsupported input was clamped to another supported level.
    pub clamped: bool,
}

impl<T: Clone> ThinkingLevelMap<T> {
    /// Returns the explicit catalog entry for one provider-neutral level.
    pub fn get(&self, level: crate::ReasoningLevel) -> Option<&LevelSupport<T>> {
        match level {
            crate::ReasoningLevel::Off => self.off.as_ref(),
            crate::ReasoningLevel::Minimal => self.minimal.as_ref(),
            crate::ReasoningLevel::Low => self.low.as_ref(),
            crate::ReasoningLevel::Medium => self.medium.as_ref(),
            crate::ReasoningLevel::High => self.high.as_ref(),
            crate::ReasoningLevel::Xhigh => self.xhigh.as_ref(),
            crate::ReasoningLevel::Max => self.max.as_ref(),
        }
    }

    /// Resolves explicit unsupported entries under strict or Pi-clamp policy.
    /// A missing entry remains missing so the API family can apply its default.
    pub fn resolve(
        &self,
        requested: crate::ReasoningLevel,
        fallback: crate::ReasoningFallback,
    ) -> Result<ReasoningLevelResolution<T>, crate::LoweringError> {
        if !matches!(self.get(requested), Some(LevelSupport::Unsupported)) {
            return Ok(ReasoningLevelResolution {
                requested,
                effective: requested,
                support: self.get(requested).cloned(),
                clamped: false,
            });
        }

        if matches!(fallback, crate::ReasoningFallback::Strict) {
            return Err(crate::LoweringError::UnsupportedReasoningLevel { requested });
        }

        const LEVELS: [crate::ReasoningLevel; 7] = [
            crate::ReasoningLevel::Off,
            crate::ReasoningLevel::Minimal,
            crate::ReasoningLevel::Low,
            crate::ReasoningLevel::Medium,
            crate::ReasoningLevel::High,
            crate::ReasoningLevel::Xhigh,
            crate::ReasoningLevel::Max,
        ];
        let requested_index = LEVELS
            .iter()
            .position(|level| *level == requested)
            .expect("all ReasoningLevel variants are listed");
        let candidates = LEVELS[requested_index + 1..]
            .iter()
            .copied()
            .chain(LEVELS[..requested_index].iter().rev().copied());
        for effective in candidates {
            let support = self.get(effective);
            let api_default_is_supported = support.is_none()
                && matches!(
                    effective,
                    crate::ReasoningLevel::Off
                        | crate::ReasoningLevel::Minimal
                        | crate::ReasoningLevel::Low
                        | crate::ReasoningLevel::Medium
                        | crate::ReasoningLevel::High
                );
            if api_default_is_supported
                || matches!(
                    support,
                    Some(LevelSupport::Disabled | LevelSupport::Value(_))
                )
            {
                return Ok(ReasoningLevelResolution {
                    requested,
                    effective,
                    support: support.cloned(),
                    clamped: true,
                });
            }
        }

        // Pinned Pi's clamp helper falls back to `off` when its supported
        // level list is empty. Represent that terminal clamp as an explicit
        // disabled plan even when the catalog itself marked `off`
        // unsupported; strict mode has already rejected above.
        Ok(ReasoningLevelResolution {
            requested,
            effective: crate::ReasoningLevel::Off,
            support: Some(LevelSupport::Disabled),
            clamped: true,
        })
    }
}

/// Versioned unknown extension value (Architecture v2 part 2 §5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionedExtension {
    /// Schema version declared by the extension owner.
    pub schema_version: u32,
    /// Exact extension JSON retained across persistence.
    pub value: Box<RawValue>,
}

impl PartialEq for VersionedExtension {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version && self.value.get() == other.value.get()
    }
}

impl Eq for VersionedExtension {}
