//! OpenAI Responses and OpenAI Codex Responses lowering, replay projection,
//! and byte-stable wire encoding.
//!
//! The provider leaf owns transport and stream decoding. This module owns the
//! two typed API-family contracts and their shared ordered output-item replay
//! encoder, as required by Architecture v2 part 2 §1.6, §3, and §10.2/§10.8.

use crate::{
    ApiFamily, ApiFamilyHandoff, AssistantMessage, CacheRetention, ContentBlock, Context,
    EncodeContext, EncodeError, HandoffError, HandoffReport, LevelSupport, LoweringError, Message,
    Modality, ModelFingerprint, OpenAiResponsesCompat, OpenAiResponsesModelConfig,
    OpenAiThinkingValue, OrderedJsonArray, OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter,
    ReasoningLevel, ReplayKind, ReplayScope, SessionAffinityFormat, SimpleGenerationOptions,
    SimpleLoweringContext, ToolCall, ToolCallId, ToolCallIdPolicy, ToolChoice, ToolResultContent,
    TypedModelDescriptor, parse_ordered_json,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// Replay kind retaining one complete OpenAI Responses reasoning output item.
pub const OPENAI_RESPONSES_REASONING_ITEM_KIND: &str = "openai.responses.reasoning-item";

/// Replay kind retaining an output-message identity and optional phase.
pub const OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND: &str = "openai.responses.message-identity";

/// Replay kind retaining function/custom-tool call provider identity.
pub const OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND: &str =
    "openai.responses.function-call-identity";

/// OpenAI's minimum accepted `max_output_tokens` value.
pub const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u32 = 16;

/// Typed view over the three OpenAI Responses replay records specified by
/// Architecture v2 part 2 §1.6.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OpenAiResponsesReplay {
    /// Complete provider reasoning output item.
    ReasoningItem {
        /// Canonical thinking block represented by the item.
        block_id: crate::ContentBlockId,
        /// `JSON.stringify`-compatible item bytes.
        item_json: Vec<u8>,
    },
    /// Provider output-message identity.
    MessageIdentity {
        /// Canonical text block represented by the item.
        block_id: crate::ContentBlockId,
        /// Provider message item ID.
        item_id: String,
        /// Optional Responses message phase.
        phase: Option<OpenAiMessagePhase>,
    },
    /// Provider function/custom-tool item identity.
    FunctionCallIdentity {
        /// Stable canonical tool call ID.
        tool_call_id: ToolCallId,
        /// Provider call ID used by the matching result item.
        call_id: String,
        /// Optional provider output-item ID.
        item_id: Option<String>,
        /// Optional deferred-tool namespace.
        namespace: Option<String>,
        /// Provider output-item kind.
        item_type: OpenAiToolItemType,
    },
}

/// OpenAI Responses output-message phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiMessagePhase {
    /// Intermediate commentary.
    Commentary,
    /// Final answer text.
    FinalAnswer,
}

impl OpenAiMessagePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

/// OpenAI Responses tool output-item kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiToolItemType {
    /// Ordinary JSON-schema function call.
    FunctionCall,
    /// Grammar-constrained custom tool call.
    CustomToolCall,
}

/// Public OpenAI Responses reasoning-summary preference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiResponsesReasoningSummary {
    /// Provider-selected summary detail.
    Auto,
    /// Detailed reasoning summary.
    Detailed,
    /// Concise reasoning summary.
    Concise,
}

impl OpenAiResponsesReasoningSummary {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Detailed => "detailed",
            Self::Concise => "concise",
        }
    }
}

/// ChatGPT Codex Responses reasoning-summary preference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCodexReasoningSummary {
    /// Provider-selected summary detail.
    Auto,
    /// Concise reasoning summary.
    Concise,
    /// Detailed reasoning summary.
    Detailed,
    /// Disable reasoning summaries.
    Off,
    /// Enable a summary with provider-selected detail.
    On,
}

impl OpenAiCodexReasoningSummary {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// Fully lowered OpenAI Responses options.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiResponsesOptions {
    /// Optional maximum output tokens.
    pub max_output_tokens: Option<u32>,
    /// Named temperature inserted before the late sampling overlay.
    pub temperature: Option<f32>,
    /// Ordered late sampling overlay.
    pub sampling: OrderedJsonObject,
    /// Positive provider-native reasoning effort, when requested.
    pub reasoning_effort: Option<String>,
    /// Optional reasoning-summary preference. The outer option distinguishes
    /// omission from an explicitly supplied nullable full option.
    pub reasoning_summary: Option<Option<OpenAiResponsesReasoningSummary>>,
    /// Optional provider service tier.
    pub service_tier: Option<String>,
    /// Optional native Responses `tool_choice` value.
    pub tool_choice: Option<OrderedJsonValue>,
    /// Prompt-cache retention preference.
    pub cache_retention: CacheRetention,
    /// Optional prompt-cache/session affinity key.
    pub session_id: Option<String>,
}

/// Dynamic simple-options patch for OpenAI Responses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiResponsesSimplePatch {
    /// Optional reasoning summary.
    pub reasoning_summary: Option<OpenAiResponsesReasoningSummary>,
    /// Optional service tier.
    pub service_tier: Option<String>,
}

/// Fully lowered OpenAI Codex Responses options.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiCodexResponsesOptions {
    /// Named temperature.
    pub temperature: Option<f32>,
    /// Positive provider-native reasoning effort, when requested.
    pub reasoning_effort: Option<String>,
    /// Reasoning summary emitted with a positive effort.
    pub reasoning_summary: Option<Option<OpenAiCodexReasoningSummary>>,
    /// Optional provider service tier.
    pub service_tier: Option<String>,
    /// Codex response verbosity.
    pub text_verbosity: OpenAiTextVerbosity,
    /// Codex tool-choice mode.
    pub tool_choice: OpenAiCodexToolChoice,
    /// Prompt-cache retention preference.
    pub cache_retention: CacheRetention,
    /// Optional prompt-cache/session affinity key.
    pub session_id: Option<String>,
}

/// Dynamic simple-options patch for OpenAI Codex Responses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCodexResponsesSimplePatch {
    /// Optional reasoning summary override.
    pub reasoning_summary: Option<OpenAiCodexReasoningSummary>,
    /// Optional service tier.
    pub service_tier: Option<String>,
    /// Optional text verbosity override.
    pub text_verbosity: Option<OpenAiTextVerbosity>,
}

/// Codex text verbosity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiTextVerbosity {
    /// Concise output; pinned Pi's default.
    #[default]
    Low,
    /// Medium verbosity.
    Medium,
    /// High verbosity.
    High,
}

impl OpenAiTextVerbosity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Codex native tool-choice domain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenAiCodexToolChoice {
    /// Let the model choose; pinned Pi's default.
    #[default]
    Auto,
    /// Disable tools.
    None,
    /// Require a tool call.
    Required,
}

impl OpenAiCodexToolChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Required => "required",
        }
    }
}

/// Marker for the `openai-responses` API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiResponses;

/// Marker for the `openai-codex-responses` API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiCodexResponses;

impl ApiFamily for OpenAiResponses {
    const API_ID: &'static str = "openai-responses";

    type Compat = OpenAiResponsesCompat;
    type ModelConfig = OpenAiResponsesModelConfig;
    type FullOptions = OpenAiResponsesOptions;
    type OptionsPatch = OpenAiResponsesSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(resolve_openai_responses_compat(
            effective_base_url,
            model_overrides,
        ))
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        lower_openai_responses_simple(context, simple, patch)
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_openai_responses(context, options)
    }
}

impl ApiFamily for OpenAiCodexResponses {
    const API_ID: &'static str = "openai-codex-responses";

    type Compat = OpenAiResponsesCompat;
    type ModelConfig = OpenAiResponsesModelConfig;
    type FullOptions = OpenAiCodexResponsesOptions;
    type OptionsPatch = OpenAiCodexResponsesSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(resolve_openai_responses_compat_without_detection(
            model_overrides,
        ))
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        lower_openai_codex_responses_simple(context, simple, patch)
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_openai_codex_responses(context, options)
    }
}

/// Resolves OpenAI Responses URL defaults, then applies typed model values.
pub fn resolve_openai_responses_compat(
    effective_base_url: &Url,
    overrides: &OpenAiResponsesCompat,
) -> OpenAiResponsesCompat {
    let openrouter = effective_base_url
        .as_str()
        .to_ascii_lowercase()
        .contains("openrouter.ai");
    let mut resolved = OpenAiResponsesCompat {
        supports_developer_role: Some(true),
        session_affinity_format: Some(if openrouter {
            SessionAffinityFormat::OpenRouter
        } else {
            SessionAffinityFormat::OpenAi
        }),
        supports_long_cache_retention: Some(true),
        supports_strict_mode: Some(false),
        supports_openai_grammar_tools: Some(false),
        supports_additional_tools: Some(false),
        supports_tool_search: Some(false),
        supports_explicit_prompt_cache_mode: Some(false),
        extensions: Default::default(),
    };
    overlay_responses_compat(&mut resolved, overrides);
    resolved
}

fn resolve_openai_responses_compat_without_detection(
    overrides: &OpenAiResponsesCompat,
) -> OpenAiResponsesCompat {
    let mut resolved = OpenAiResponsesCompat {
        supports_developer_role: Some(true),
        session_affinity_format: Some(SessionAffinityFormat::OpenAi),
        supports_long_cache_retention: Some(true),
        supports_strict_mode: Some(true),
        supports_openai_grammar_tools: Some(false),
        supports_additional_tools: Some(false),
        supports_tool_search: Some(false),
        supports_explicit_prompt_cache_mode: Some(false),
        extensions: Default::default(),
    };
    overlay_responses_compat(&mut resolved, overrides);
    resolved
}

fn overlay_responses_compat(target: &mut OpenAiResponsesCompat, overrides: &OpenAiResponsesCompat) {
    macro_rules! overlay {
        ($field:ident) => {
            if overrides.$field.is_some() {
                target.$field = overrides.$field;
            }
        };
    }
    overlay!(supports_developer_role);
    overlay!(session_affinity_format);
    overlay!(supports_long_cache_retention);
    overlay!(supports_strict_mode);
    overlay!(supports_openai_grammar_tools);
    overlay!(supports_additional_tools);
    overlay!(supports_tool_search);
    overlay!(supports_explicit_prompt_cache_mode);
    target.extensions.extend(overrides.extensions.clone());
}

fn lower_openai_responses_simple(
    context: SimpleLoweringContext<'_, OpenAiResponses>,
    simple: &SimpleGenerationOptions,
    patch: &OpenAiResponsesSimplePatch,
) -> Result<OpenAiResponsesOptions, LoweringError> {
    let maximum = lower_responses_maximum(context.model, context.available_context_tokens, simple);
    let sampling = lower_responses_sampling(context.model, simple);
    let reasoning_effort = lower_responses_reasoning(
        &context.model.config,
        simple.reasoning,
        simple.reasoning_fallback,
    )?;
    Ok(OpenAiResponsesOptions {
        max_output_tokens: Some(maximum),
        temperature: simple.temperature,
        sampling,
        reasoning_effort,
        reasoning_summary: patch.reasoning_summary.map(Some),
        service_tier: patch.service_tier.clone(),
        tool_choice: simple.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => OrderedJsonValue::from("auto"),
            ToolChoice::None => OrderedJsonValue::from("none"),
        }),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
    })
}

fn lower_openai_codex_responses_simple(
    context: SimpleLoweringContext<'_, OpenAiCodexResponses>,
    simple: &SimpleGenerationOptions,
    patch: &OpenAiCodexResponsesSimplePatch,
) -> Result<OpenAiCodexResponsesOptions, LoweringError> {
    let reasoning_effort = lower_responses_reasoning(
        &context.model.config,
        simple.reasoning,
        simple.reasoning_fallback,
    )?;
    Ok(OpenAiCodexResponsesOptions {
        temperature: simple.temperature,
        reasoning_effort,
        reasoning_summary: patch.reasoning_summary.map(Some),
        service_tier: patch.service_tier.clone(),
        text_verbosity: patch.text_verbosity.unwrap_or_default(),
        tool_choice: match simple.tool_choice.unwrap_or_default() {
            ToolChoice::Auto => OpenAiCodexToolChoice::Auto,
            ToolChoice::None => OpenAiCodexToolChoice::None,
        },
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
    })
}

fn lower_responses_maximum<A: ApiFamily<ModelConfig = OpenAiResponsesModelConfig>>(
    model: &TypedModelDescriptor<A>,
    available_context_tokens: u64,
    simple: &SimpleGenerationOptions,
) -> u32 {
    let requested = simple
        .max_output_tokens
        .unwrap_or(model.common.limits.max_output_tokens);
    if model.common.limits.context_window == 0 {
        requested.max(1)
    } else {
        requested.min(u32::try_from(available_context_tokens.max(1)).unwrap_or(u32::MAX))
    }
}

fn lower_responses_sampling<A: ApiFamily<ModelConfig = OpenAiResponsesModelConfig>>(
    model: &TypedModelDescriptor<A>,
    simple: &SimpleGenerationOptions,
) -> OrderedJsonObject {
    let mut sampling = model.config.sampling_defaults.clone();
    for (name, value) in &simple.sampling {
        sampling.insert(name.clone(), value.clone());
    }
    if let Some(top_p) = simple.top_p
        && sampling.get("top_p").is_none()
    {
        sampling.insert("top_p", top_p);
    }
    if let Some(seed) = simple.seed
        && sampling.get("seed").is_none()
    {
        sampling.insert("seed", seed);
    }
    if !simple.stop.is_empty() && sampling.get("stop").is_none() {
        sampling.insert(
            "stop",
            simple
                .stop
                .iter()
                .map(String::as_str)
                .collect::<OrderedJsonArray>(),
        );
    }
    sampling
}

fn lower_responses_reasoning(
    config: &OpenAiResponsesModelConfig,
    requested: Option<ReasoningLevel>,
    fallback: crate::ReasoningFallback,
) -> Result<Option<String>, LoweringError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    // Pinned Pi's simple Responses adapters clamp first and then erase an
    // explicit `off` before entering the full-options encoder. Public
    // Responses may still apply its model/provider-specific no-reasoning
    // default, while Codex must omit the `reasoning` object entirely.
    if requested == ReasoningLevel::Off {
        return Ok(None);
    }
    let resolution = config.thinking_levels.resolve(requested, fallback)?;
    Ok(match resolution.support {
        Some(LevelSupport::Unsupported | LevelSupport::Disabled) => None,
        Some(LevelSupport::Value(OpenAiThinkingValue::Disabled)) => None,
        Some(LevelSupport::Value(OpenAiThinkingValue::Effort(_))) => {
            Some(reasoning_level_name(resolution.effective).into())
        }
        Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(_))) => {
            return Err(LoweringError::InvalidConfiguration {
                message: "OpenAI Responses reasoning levels cannot map to token budgets".into(),
            });
        }
        None => Some(reasoning_level_name(resolution.effective).into()),
    })
}

fn reasoning_level_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::Xhigh => "xhigh",
        ReasoningLevel::Max => "max",
    }
}

/// Encodes one byte-stable OpenAI Responses request body.
pub fn encode_openai_responses(
    context: EncodeContext<'_, OpenAiResponses>,
    options: &OpenAiResponsesOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let deferred_mode = responses_deferred_tools_mode(context.compat);
    let (immediate_tools, deferred_tools) =
        split_responses_deferred_tools(context.context, deferred_mode.is_some());
    let input = convert_responses_messages(
        context.model,
        context.context,
        context.compat,
        true,
        false,
        deferred_mode,
        &deferred_tools,
    )?;
    let mut request = OrderedJsonObject::new();
    request.insert("model", context.model.common.model_ref.model.as_str());
    request.insert("input", input);
    request.insert("stream", true);
    if options.cache_retention != CacheRetention::None
        && let Some(key) = crate::clamp_openai_prompt_cache_key(options.session_id.as_deref())
    {
        request.insert("prompt_cache_key", key);
    }
    if options.cache_retention == CacheRetention::Long
        && context.compat.supports_long_cache_retention.unwrap_or(true)
    {
        request.insert("prompt_cache_retention", "24h");
    }
    if options.cache_retention == CacheRetention::None
        && context
            .compat
            .supports_explicit_prompt_cache_mode
            .unwrap_or(false)
    {
        request.insert(
            "prompt_cache_options",
            OrderedJsonObject::from_iter([("mode", OrderedJsonValue::from("explicit"))]),
        );
    }
    request.insert("store", false);

    if let Some(maximum) = options.max_output_tokens.filter(|maximum| *maximum > 0) {
        request.insert(
            "max_output_tokens",
            maximum.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS),
        );
    }
    if let Some(temperature) = options.temperature {
        request.insert("temperature", temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        request.insert("service_tier", service_tier.as_str());
    }
    if !immediate_tools.is_empty() {
        request.insert(
            "tools",
            encode_responses_tools(&immediate_tools, context.compat, false, false)?,
        );
    }
    if let Some(tool_choice) = &options.tool_choice {
        request.insert("tool_choice", tool_choice.clone());
    }

    if context.model.common.reasoning {
        if options.reasoning_effort.is_some()
            || options
                .reasoning_summary
                .as_ref()
                .and_then(|value| *value)
                .is_some()
        {
            let mut reasoning = OrderedJsonObject::new();
            let effort = match options.reasoning_effort.as_deref() {
                Some(effort) => map_responses_full_reasoning_effort(&context.model.config, effort)?,
                None => "medium".to_owned(),
            };
            reasoning.insert("effort", effort);
            reasoning.insert(
                "summary",
                options
                    .reasoning_summary
                    .as_ref()
                    .and_then(|value| *value)
                    .unwrap_or(OpenAiResponsesReasoningSummary::Auto)
                    .as_str(),
            );
            request.insert("reasoning", reasoning);
            request.insert(
                "include",
                OrderedJsonArray::from_iter([OrderedJsonValue::from(
                    "reasoning.encrypted_content",
                )]),
            );
        } else if context.model.common.model_ref.provider.as_str() != "github-copilot"
            && let Some(effort) = off_reasoning_effort(&context.model.config)
        {
            request.insert(
                "reasoning",
                OrderedJsonObject::from_iter([("effort", OrderedJsonValue::from(effort))]),
            );
        }
        if context.model.common.model_ref.provider.as_str() == "xai" {
            request.insert(
                "include",
                OrderedJsonArray::from_iter([OrderedJsonValue::from(
                    "reasoning.encrypted_content",
                )]),
            );
        }
    }
    for (key, value) in &options.sampling {
        request.insert(key.clone(), value.clone());
    }
    Ok(request)
}

/// Encodes one byte-stable ChatGPT Codex Responses request body.
pub fn encode_openai_codex_responses(
    context: EncodeContext<'_, OpenAiCodexResponses>,
    options: &OpenAiCodexResponsesOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let deferred_mode = responses_deferred_tools_mode(context.compat);
    let (immediate_tools, deferred_tools) =
        split_responses_deferred_tools(context.context, deferred_mode.is_some());
    let input = convert_responses_messages(
        context.model,
        context.context,
        context.compat,
        false,
        true,
        deferred_mode,
        &deferred_tools,
    )?;
    let mut request = OrderedJsonObject::new();
    request.insert("model", context.model.common.model_ref.model.as_str());
    request.insert("store", false);
    request.insert("stream", true);
    request.insert(
        "instructions",
        context
            .context
            .system_prompt
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("You are a helpful assistant."),
    );
    request.insert("input", input);
    request.insert(
        "text",
        OrderedJsonObject::from_iter([(
            "verbosity",
            OrderedJsonValue::from(options.text_verbosity.as_str()),
        )]),
    );
    request.insert(
        "include",
        OrderedJsonArray::from_iter([OrderedJsonValue::from("reasoning.encrypted_content")]),
    );
    if options.cache_retention != CacheRetention::None
        && let Some(key) = crate::clamp_openai_prompt_cache_key(options.session_id.as_deref())
    {
        request.insert("prompt_cache_key", key);
    }
    request.insert("tool_choice", options.tool_choice.as_str());
    request.insert("parallel_tool_calls", true);
    if let Some(temperature) = options.temperature {
        request.insert("temperature", temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        request.insert("service_tier", service_tier.as_str());
    }
    if !immediate_tools.is_empty() {
        request.insert(
            "tools",
            encode_responses_tools(&immediate_tools, context.compat, true, false)?,
        );
    }
    if let Some(effort) = options
        .reasoning_effort
        .as_deref()
        .map(|effort| map_responses_full_reasoning_effort(&context.model.config, effort))
        .transpose()?
    {
        request.insert(
            "reasoning",
            OrderedJsonObject::from_iter([
                ("effort", OrderedJsonValue::from(effort)),
                (
                    "summary",
                    OrderedJsonValue::from(
                        options
                            .reasoning_summary
                            .as_ref()
                            .and_then(|value| *value)
                            .unwrap_or(OpenAiCodexReasoningSummary::Auto)
                            .as_str(),
                    ),
                ),
            ]),
        );
    }
    Ok(request)
}

fn map_responses_full_reasoning_effort(
    config: &OpenAiResponsesModelConfig,
    requested: &str,
) -> Result<String, EncodeError> {
    let configured = match requested {
        "none" => config.thinking_levels.off.as_ref(),
        "minimal" => config.thinking_levels.minimal.as_ref(),
        "low" => config.thinking_levels.low.as_ref(),
        "medium" => config.thinking_levels.medium.as_ref(),
        "high" => config.thinking_levels.high.as_ref(),
        "xhigh" => config.thinking_levels.xhigh.as_ref(),
        "max" => config.thinking_levels.max.as_ref(),
        _ => return Ok(requested.to_owned()),
    };
    match configured {
        None
        | Some(LevelSupport::Unsupported | LevelSupport::Disabled)
        | Some(LevelSupport::Value(OpenAiThinkingValue::Disabled)) => Ok(requested.to_owned()),
        Some(LevelSupport::Value(OpenAiThinkingValue::Effort(effort))) => Ok(effort.clone()),
        Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(_))) => {
            Err(EncodeError::InvalidRequest {
                message: "OpenAI Responses reasoning levels cannot map to token budgets".into(),
            })
        }
    }
}

fn off_reasoning_effort(config: &OpenAiResponsesModelConfig) -> Option<&str> {
    match config.thinking_levels.off.as_ref() {
        Some(LevelSupport::Unsupported) => None,
        Some(LevelSupport::Value(OpenAiThinkingValue::Effort(value))) => Some(value.as_str()),
        Some(LevelSupport::Disabled | LevelSupport::Value(OpenAiThinkingValue::Disabled))
        | None => Some("none"),
        Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(_))) => None,
    }
}

#[derive(Deserialize)]
struct MessageIdentity {
    id: String,
    #[serde(default)]
    phase: Option<OpenAiMessagePhase>,
    block_id: crate::ContentBlockId,
}

#[derive(Deserialize)]
struct FunctionCallIdentity {
    tool_call_id: ToolCallId,
    call_id: String,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(rename = "type")]
    item_type: OpenAiToolItemType,
}

fn convert_responses_messages<
    A: ApiFamily<Compat = OpenAiResponsesCompat, ModelConfig = OpenAiResponsesModelConfig>,
>(
    model: &TypedModelDescriptor<A>,
    context: &Context,
    compat: &OpenAiResponsesCompat,
    include_system_prompt: bool,
    codex: bool,
    deferred_mode: Option<ResponsesDeferredToolsMode>,
    deferred_tools: &BTreeMap<String, crate::ToolSpec>,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut output = OrderedJsonArray::new();
    if include_system_prompt
        && let Some(prompt) = context
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
    {
        output.push(OrderedJsonObject::from_iter([
            (
                "role",
                OrderedJsonValue::from(
                    if model.common.reasoning && compat.supports_developer_role.unwrap_or(true) {
                        "developer"
                    } else {
                        "system"
                    },
                ),
            ),
            (
                "content",
                OrderedJsonValue::from(sanitize_responses_text(prompt)),
            ),
        ]));
    }

    let grammar_properties = responses_grammar_tool_input_properties(context, compat)?;
    let target = ReplayScope::new(
        model.common.model_ref.provider.clone(),
        A::API_ID,
        model.common.model_ref.model.clone(),
        model.common.model_ref.model.clone(),
    );
    let mut loaded_tool_names = BTreeSet::new();
    let mut message_index = 0_usize;
    for message in &context.messages {
        let output_len = output.len();
        match message {
            Message::User(message) => {
                let content = encode_user_content(&message.content);
                if !content.is_empty() {
                    output.push(OrderedJsonObject::from_iter([
                        ("role", OrderedJsonValue::from("user")),
                        ("content", OrderedJsonValue::from(content)),
                    ]));
                }
            }
            Message::Assistant(message) => encode_responses_assistant(
                message,
                &target,
                &grammar_properties,
                deferred_tools,
                message_index,
                &mut output,
            )?,
            Message::ToolResult(result) => {
                let custom = grammar_properties.contains_key(&result.tool_name);
                let call_id = result
                    .tool_call_id
                    .as_str()
                    .split_once('|')
                    .map_or(result.tool_call_id.as_str(), |(call_id, _)| call_id);
                let mut item = OrderedJsonObject::new();
                item.insert(
                    "type",
                    if custom {
                        "custom_tool_call_output"
                    } else {
                        "function_call_output"
                    },
                );
                item.insert("call_id", call_id);
                item.insert("output", encode_tool_result_output(model, &result.content));
                output.push(item);

                let loaded = result
                    .added_tool_names
                    .iter()
                    .filter_map(|name| {
                        if loaded_tool_names.insert(name.clone()) {
                            deferred_tools.get(name).cloned()
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if !loaded.is_empty() {
                    match deferred_mode {
                        Some(ResponsesDeferredToolsMode::AdditionalTools) => {
                            output.push(OrderedJsonObject::from_iter([
                                ("type", OrderedJsonValue::from("additional_tools")),
                                ("role", OrderedJsonValue::from("developer")),
                                (
                                    "tools",
                                    OrderedJsonValue::from(encode_responses_tools(
                                        &loaded, compat, codex, false,
                                    )?),
                                ),
                            ]));
                        }
                        Some(ResponsesDeferredToolsMode::ToolSearch) => {
                            let names = loaded
                                .iter()
                                .map(|tool| tool.name.as_str())
                                .collect::<Vec<_>>();
                            let search_call_id = format!(
                                "pi_tool_load_{}",
                                short_hash(&format!("{}:{}", result.tool_call_id, names.join(",")))
                            );
                            output.push(OrderedJsonObject::from_iter([
                                ("type", OrderedJsonValue::from("tool_search_call")),
                                ("call_id", OrderedJsonValue::from(search_call_id.as_str())),
                                ("execution", OrderedJsonValue::from("client")),
                                ("status", OrderedJsonValue::from("completed")),
                                (
                                    "arguments",
                                    OrderedJsonValue::from(OrderedJsonObject::from_iter([
                                        ("query", OrderedJsonValue::from(names.join(" "))),
                                        (
                                            "limit",
                                            OrderedJsonValue::from(
                                                u64::try_from(names.len()).unwrap_or(u64::MAX),
                                            ),
                                        ),
                                    ])),
                                ),
                            ]));
                            output.push(OrderedJsonObject::from_iter([
                                ("type", OrderedJsonValue::from("tool_search_output")),
                                ("call_id", OrderedJsonValue::from(search_call_id)),
                                ("execution", OrderedJsonValue::from("client")),
                                ("status", OrderedJsonValue::from("completed")),
                                (
                                    "tools",
                                    OrderedJsonValue::from(encode_responses_tools(
                                        &loaded, compat, codex, true,
                                    )?),
                                ),
                            ]));
                        }
                        None => {}
                    }
                }
            }
        }
        if output.len() > output_len {
            message_index += 1;
        }
    }
    Ok(output)
}

fn encode_user_content(content: &[ContentBlock]) -> OrderedJsonArray {
    let mut output = OrderedJsonArray::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => output.push(OrderedJsonObject::from_iter([
                ("type", OrderedJsonValue::from("input_text")),
                (
                    "text",
                    OrderedJsonValue::from(sanitize_responses_text(text)),
                ),
            ])),
            ContentBlock::Image {
                data, mime_type, ..
            } => output.push(input_image(data, mime_type)),
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => {}
        }
    }
    output
}

fn encode_responses_assistant(
    message: &AssistantMessage,
    target: &ReplayScope,
    grammar_properties: &BTreeMap<String, String>,
    deferred_tools: &BTreeMap<String, crate::ToolSpec>,
    message_index: usize,
    output: &mut OrderedJsonArray,
) -> Result<(), EncodeError> {
    let mut replay = message.replay.items.iter().enumerate().collect::<Vec<_>>();
    replay.sort_by_key(|(position, item)| (item.ordinal, *position));

    // Pi walks projected canonical blocks and consults the replay identity for
    // each block in place. That distinction matters after a cross-model
    // projection: reasoning and message identities are model-scoped and may be
    // dropped while a deferred function-call identity remains API-scoped. A
    // replay-first pass would incorrectly move that call ahead of the
    // canonical thinking/text fallbacks.
    let mut reasoning_items = BTreeMap::new();
    let mut message_items = BTreeMap::new();
    let mut function_items = BTreeMap::new();
    for (_, item) in replay {
        if !message.replay.is_complete_and_applicable(item, target) {
            continue;
        }
        let Some(typed) = decode_openai_responses_replay(message, item)? else {
            continue;
        };
        match typed {
            OpenAiResponsesReplay::ReasoningItem {
                block_id,
                item_json,
            } => {
                let value =
                    parse_ordered_json(std::str::from_utf8(&item_json).map_err(|error| {
                        EncodeError::InvalidRequest {
                            message: format!("OpenAI reasoning replay is not UTF-8: {error}"),
                        }
                    })?)
                    .map_err(|error| EncodeError::InvalidRequest {
                        message: format!("invalid OpenAI reasoning replay JSON: {error}"),
                    })?;
                reasoning_items.entry(block_id).or_insert(value);
            }
            OpenAiResponsesReplay::MessageIdentity {
                block_id,
                item_id,
                phase,
            } => {
                message_items
                    .entry(block_id.clone())
                    .or_insert(MessageIdentity {
                        id: item_id,
                        phase,
                        block_id,
                    });
            }
            OpenAiResponsesReplay::FunctionCallIdentity {
                tool_call_id,
                call_id,
                item_id,
                namespace,
                item_type,
            } => {
                function_items
                    .entry(tool_call_id.clone())
                    .or_insert(FunctionCallIdentity {
                        tool_call_id,
                        call_id,
                        item_id,
                        namespace,
                        item_type,
                    });
            }
        }
    }

    let mut text_index = 0_usize;
    for block in &message.content {
        match block {
            ContentBlock::Thinking { id, .. } => {
                if let Some(item) = reasoning_items.remove(id) {
                    output.push(item);
                }
            }
            ContentBlock::Text { id, text } => {
                let fallback_id = if text_index == 0 {
                    format!("msg_pi_{message_index}")
                } else {
                    format!("msg_pi_{message_index}_{text_index}")
                };
                text_index += 1;
                if let Some(identity) = message_items.remove(id) {
                    output.push(output_message(&identity.id, identity.phase, text));
                } else {
                    output.push(output_message(&fallback_id, None, text));
                }
            }
            ContentBlock::ToolCall { call, .. } => {
                if let Some(mut identity) = function_items.remove(&call.id) {
                    let same_model = message.provider == target.provider
                        && message.api == target.api
                        && message
                            .response_model
                            .as_ref()
                            .unwrap_or(&message.requested_model)
                            == &target.requested_model;
                    if !same_model
                        && identity
                            .item_id
                            .as_deref()
                            .is_some_and(|item_id| item_id.starts_with("fc_"))
                    {
                        identity.item_id = None;
                    }
                    if !same_model && !deferred_tools.contains_key(&call.name) {
                        identity.namespace = None;
                    }
                    output.push(encode_responses_tool_call(
                        call,
                        &identity,
                        grammar_properties,
                    )?);
                    continue;
                }
                let mut id_parts = call.id.as_str().split('|');
                let call_id = id_parts.next().unwrap_or_default();
                let item_id = id_parts.next();
                let same_provider_api =
                    message.provider == target.provider && message.api == target.api;
                let same_model = same_provider_api
                    && message
                        .response_model
                        .as_ref()
                        .unwrap_or(&message.requested_model)
                        == &target.requested_model;
                let custom = grammar_properties.contains_key(&call.name);
                let item_id = item_id.filter(|item_id| {
                    !(!same_model && same_provider_api && item_id.starts_with("fc_"))
                        && (custom || item_id.starts_with("fc_"))
                });
                let identity = FunctionCallIdentity {
                    tool_call_id: call.id.clone(),
                    call_id: call_id.to_owned(),
                    item_id: item_id.map(str::to_owned),
                    namespace: None,
                    item_type: if custom {
                        OpenAiToolItemType::CustomToolCall
                    } else {
                        OpenAiToolItemType::FunctionCall
                    },
                };
                output.push(encode_responses_tool_call(
                    call,
                    &identity,
                    grammar_properties,
                )?);
            }
            ContentBlock::Image { .. } => {}
        }
    }
    Ok(())
}

fn output_message(id: &str, phase: Option<OpenAiMessagePhase>, text: &str) -> OrderedJsonObject {
    let normalized_id = if id.encode_utf16().count() > 64 {
        format!("msg_{}", short_hash(id))
    } else {
        id.to_owned()
    };
    let mut message = OrderedJsonObject::new();
    message.insert("type", "message");
    message.insert("role", "assistant");
    message.insert(
        "content",
        OrderedJsonArray::from_iter([OrderedJsonValue::from(OrderedJsonObject::from_iter([
            ("type", OrderedJsonValue::from("output_text")),
            (
                "text",
                OrderedJsonValue::from(sanitize_responses_text(text)),
            ),
            (
                "annotations",
                OrderedJsonValue::from(OrderedJsonArray::new()),
            ),
        ]))]),
    );
    message.insert("status", "completed");
    message.insert("id", normalized_id);
    if let Some(phase) = phase {
        message.insert("phase", phase.as_str());
    }
    message
}

fn encode_responses_tool_call(
    call: &ToolCall,
    identity: &FunctionCallIdentity,
    grammar_properties: &BTreeMap<String, String>,
) -> Result<OrderedJsonObject, EncodeError> {
    let custom_property = grammar_properties.get(&call.name);
    let custom =
        identity.item_type == OpenAiToolItemType::CustomToolCall && custom_property.is_some();
    let mut output = OrderedJsonObject::new();
    output.insert(
        "type",
        if custom {
            "custom_tool_call"
        } else {
            "function_call"
        },
    );
    if let Some(id) = identity.item_id.as_deref() {
        output.insert("id", id);
    }
    output.insert("call_id", identity.call_id.as_str());
    output.insert("name", call.name.as_str());
    if custom {
        let property = custom_property.expect("checked above");
        let input = call
            .arguments
            .get(property)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| EncodeError::InvalidRequest {
                message: format!(
                    "Grammar tool call \"{}\" requires argument \"{property}\" to be a string.",
                    call.name
                ),
            })?;
        output.insert("input", sanitize_responses_text(input));
    } else {
        output.insert("arguments", stringify_json(&call.arguments)?);
    }
    if let Some(namespace) = identity.namespace.as_deref() {
        output.insert("namespace", namespace);
    }
    Ok(output)
}

fn encode_tool_result_output<A: ApiFamily>(
    model: &TypedModelDescriptor<A>,
    content: &[ToolResultContent],
) -> OrderedJsonValue {
    let text = content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text { text, .. } => Some(text.as_str()),
            ToolResultContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Image {
                data, mime_type, ..
            } => Some((data, mime_type)),
            ToolResultContent::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() || !model.common.modalities.input.contains(&Modality::Image) {
        return OrderedJsonValue::from(sanitize_responses_text(if !text.is_empty() {
            &text
        } else if images.is_empty() {
            "(no tool output)"
        } else {
            "(see attached image)"
        }));
    }
    let mut output = OrderedJsonArray::new();
    if !text.is_empty() {
        output.push(OrderedJsonObject::from_iter([
            ("type", OrderedJsonValue::from("input_text")),
            (
                "text",
                OrderedJsonValue::from(sanitize_responses_text(&text)),
            ),
        ]));
    }
    for (data, mime_type) in images {
        output.push(input_image(data, mime_type));
    }
    output.into()
}

fn input_image(data: &str, mime_type: &str) -> OrderedJsonObject {
    OrderedJsonObject::from_iter([
        ("type", OrderedJsonValue::from("input_image")),
        ("detail", OrderedJsonValue::from("auto")),
        (
            "image_url",
            OrderedJsonValue::from(format!("data:{mime_type};base64,{data}")),
        ),
    ])
}

fn decode_replay_json<T: for<'de> Deserialize<'de>>(
    item: &crate::ReplayItem,
) -> Result<T, EncodeError> {
    serde_json::from_slice(
        item.json_bytes()
            .map_err(|error| EncodeError::InvalidRequest {
                message: error.to_string(),
            })?,
    )
    .map_err(|error| EncodeError::InvalidRequest {
        message: format!("invalid {} replay payload: {error}", item.kind),
    })
}

fn decode_openai_responses_replay(
    message: &AssistantMessage,
    item: &crate::ReplayItem,
) -> Result<Option<OpenAiResponsesReplay>, EncodeError> {
    match item.kind.as_str() {
        OPENAI_RESPONSES_REASONING_ITEM_KIND => {
            let linked_block_id = message.content.iter().find_map(|block| match block {
                ContentBlock::Thinking {
                    id,
                    replay_item: Some(replay_item),
                    ..
                } if replay_item == &item.id => Some(id.clone()),
                _ => None,
            });
            let targeted_block_id = match &item.target {
                crate::ReplayTarget::ContentBlock(block_id)
                    if message.content.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::Thinking { id, .. } if id == block_id
                        )
                    }) =>
                {
                    Some(block_id.clone())
                }
                _ => None,
            };
            let block_id = match (linked_block_id, targeted_block_id) {
                (Some(linked), Some(targeted)) if linked != targeted => {
                    return Err(EncodeError::InvalidRequest {
                        message: format!(
                            "OpenAI reasoning replay {} targets content block {targeted} but is linked to {linked}",
                            item.id
                        ),
                    });
                }
                (Some(linked), _) => linked,
                (None, Some(targeted)) => targeted,
                (None, None) => return Ok(None),
            };
            Ok(Some(OpenAiResponsesReplay::ReasoningItem {
                block_id,
                item_json: item
                    .json_bytes()
                    .map_err(|error| EncodeError::InvalidRequest {
                        message: error.to_string(),
                    })?
                    .to_vec(),
            }))
        }
        OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND => {
            let identity: MessageIdentity = decode_replay_json(item)?;
            Ok(Some(OpenAiResponsesReplay::MessageIdentity {
                block_id: identity.block_id,
                item_id: identity.id,
                phase: identity.phase,
            }))
        }
        OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND => {
            let identity: FunctionCallIdentity = decode_replay_json(item)?;
            Ok(Some(OpenAiResponsesReplay::FunctionCallIdentity {
                tool_call_id: identity.tool_call_id,
                call_id: identity.call_id,
                item_id: identity.item_id,
                namespace: identity.namespace,
                item_type: identity.item_type,
            }))
        }
        _ => Ok(None),
    }
}

fn stringify_json(value: &serde_json::Value) -> Result<String, EncodeError> {
    OrderedJsonWriter::stringify(&OrderedJsonValue::from(value.clone())).map_err(|error| {
        EncodeError::InvalidRequest {
            message: format!("failed to encode tool arguments: {error}"),
        }
    })
}

fn encode_responses_tools(
    tools: &[crate::ToolSpec],
    compat: &OpenAiResponsesCompat,
    codex: bool,
    defer_loading: bool,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut output = OrderedJsonArray::new();
    for tool in tools {
        if let Some(grammar) = resolve_responses_grammar(tool, compat)? {
            let mut encoded = OrderedJsonObject::new();
            encoded.insert("type", "custom");
            encoded.insert("name", tool.name.as_str());
            encoded.insert("description", tool.description.as_str());
            encoded.insert(
                "format",
                OrderedJsonObject::from_iter([
                    ("type", OrderedJsonValue::from("grammar")),
                    ("syntax", OrderedJsonValue::from(grammar.syntax)),
                    (
                        "definition",
                        OrderedJsonValue::from(grammar.definition.as_str()),
                    ),
                ]),
            );
            if defer_loading {
                encoded.insert("defer_loading", true);
            }
            output.push(encoded);
            continue;
        }
        let (parameters, strict) = resolve_responses_strict_tool(tool, compat)?;
        let mut encoded = OrderedJsonObject::new();
        encoded.insert("type", "function");
        encoded.insert("name", tool.name.as_str());
        encoded.insert("description", tool.description.as_str());
        encoded.insert("parameters", OrderedJsonValue::from(parameters));
        if defer_loading {
            encoded.insert("defer_loading", true);
        }
        if compat.supports_strict_mode.unwrap_or(!codex) {
            match strict {
                Some(value) => encoded.insert("strict", value),
                None if codex => encoded.insert("strict", OrderedJsonValue::Null),
                None => encoded.insert("strict", false),
            };
        }
        output.push(encoded);
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponsesDeferredToolsMode {
    AdditionalTools,
    ToolSearch,
}

fn responses_deferred_tools_mode(
    compat: &OpenAiResponsesCompat,
) -> Option<ResponsesDeferredToolsMode> {
    if compat.supports_additional_tools.unwrap_or(false) {
        Some(ResponsesDeferredToolsMode::AdditionalTools)
    } else if compat.supports_tool_search.unwrap_or(false) {
        Some(ResponsesDeferredToolsMode::ToolSearch)
    } else {
        None
    }
}

fn split_responses_deferred_tools(
    context: &Context,
    enabled: bool,
) -> (Vec<crate::ToolSpec>, BTreeMap<String, crate::ToolSpec>) {
    let mut unique_tools = Vec::<crate::ToolSpec>::new();
    for tool in &context.tools {
        if let Some(existing) = unique_tools.iter_mut().find(|item| item.name == tool.name) {
            *existing = tool.clone();
        } else {
            unique_tools.push(tool.clone());
        }
    }
    if !enabled {
        return (unique_tools, BTreeMap::new());
    }

    let mut used_names = BTreeSet::new();
    let mut deferred_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(message) => {
                for block in &message.content {
                    if let ContentBlock::ToolCall { call, .. } = block {
                        used_names.insert(call.name.clone());
                    }
                }
            }
            Message::ToolResult(message) => {
                for name in &message.added_tool_names {
                    if !used_names.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = BTreeMap::new();
    for tool in unique_tools {
        if deferred_names.contains(&tool.name) {
            deferred.insert(tool.name.clone(), tool);
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

struct ResponsesGrammar {
    syntax: &'static str,
    definition: String,
    input_property: String,
}

fn resolve_responses_grammar(
    tool: &crate::ToolSpec,
    compat: &OpenAiResponsesCompat,
) -> Result<Option<ResponsesGrammar>, EncodeError> {
    let Some(crate::ConstrainedSampling::Config(crate::ConstrainedSamplingConfig::Grammar {
        variants,
    })) = &tool.constrained_sampling
    else {
        return Ok(None);
    };
    if !compat.supports_openai_grammar_tools.unwrap_or(false) {
        return Ok(None);
    }
    let (syntax, definition) = variants
        .get(&crate::GrammarFormat::OpenAiLark)
        .filter(|value| !value.trim().is_empty())
        .map(|value| ("lark", value))
        .or_else(|| {
            variants
                .get(&crate::GrammarFormat::OpenAiRegex)
                .filter(|value| !value.trim().is_empty())
                .map(|value| ("regex", value))
        })
        .ok_or_else(|| EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided",
                tool.name
            ),
        })?;
    let input_property = infer_responses_grammar_property(&tool.parameters).map_err(|message| {
        EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" cannot use grammar constrained sampling: {message}",
                tool.name
            ),
        }
    })?;
    Ok(Some(ResponsesGrammar {
        syntax,
        definition: definition.clone(),
        input_property,
    }))
}

fn infer_responses_grammar_property(schema: &serde_json::Value) -> Result<String, String> {
    let schema = schema.as_object().ok_or_else(|| {
        "grammar constrained sampling requires an object parameter schema".to_owned()
    })?;
    if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".into());
    }
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .filter(|required| required.len() == 1)
        .and_then(|required| required[0].as_str())
        .ok_or_else(|| {
            "grammar constrained sampling requires exactly one required string property".to_owned()
        })?;
    let property = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .and_then(|properties| properties.get(required))
        .ok_or_else(|| {
            format!("grammar constrained sampling requires a properties entry for {required}")
        })?;
    if property.get("type").and_then(serde_json::Value::as_str) != Some("string") {
        return Err(format!(
            "grammar constrained sampling property {required} must have type string"
        ));
    }
    Ok(required.to_owned())
}

fn resolve_responses_strict_tool(
    tool: &crate::ToolSpec,
    compat: &OpenAiResponsesCompat,
) -> Result<(serde_json::Value, Option<bool>), EncodeError> {
    let Some(crate::ConstrainedSampling::Config(crate::ConstrainedSamplingConfig::JsonSchema {
        strict,
    })) = tool.constrained_sampling
    else {
        return Ok((tool.parameters.clone(), None));
    };
    if !compat.supports_strict_mode.unwrap_or(false) {
        return match strict {
            crate::JsonSchemaStrictMode::Prefer => Ok((tool.parameters.clone(), None)),
            crate::JsonSchemaStrictMode::Require => Err(EncodeError::InvalidRequest {
                message: format!(
                    "tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported",
                    tool.name
                ),
            }),
        };
    }
    match crate::openai_completions::make_strict_json_schema(&tool.parameters) {
        Ok(parameters) => Ok((parameters, Some(true))),
        Err(_) if strict == crate::JsonSchemaStrictMode::Prefer => {
            Ok((tool.parameters.clone(), None))
        }
        Err(message) => Err(EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" requires JSON-schema constrained sampling, but {message}",
                tool.name
            ),
        }),
    }
}

/// Resolves grammar-tool input properties for a Responses request and decoder.
pub fn responses_grammar_tool_input_properties(
    context: &Context,
    compat: &OpenAiResponsesCompat,
) -> Result<BTreeMap<String, String>, EncodeError> {
    let mut properties = BTreeMap::new();
    for tool in &context.tools {
        if let Some(grammar) = resolve_responses_grammar(tool, compat)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

/// Adds OpenAI Responses session-affinity defaults before explicit request
/// headers and final header transforms.
pub(crate) fn apply_openai_responses_session_affinity_headers(
    effective_base_url: &Url,
    compat_overrides: &OpenAiResponsesCompat,
    options: &SimpleGenerationOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    let compat = resolve_openai_responses_compat(effective_base_url, compat_overrides);
    apply_responses_affinity(
        compat
            .session_affinity_format
            .unwrap_or(SessionAffinityFormat::OpenAi),
        options.cache_retention.unwrap_or_default(),
        options.session_id.as_deref(),
        headers,
    )
}

/// Adds affinity headers for fully typed OpenAI Responses options.
pub fn apply_openai_responses_full_headers(
    compat: &OpenAiResponsesCompat,
    options: &OpenAiResponsesOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    apply_responses_affinity(
        compat
            .session_affinity_format
            .unwrap_or(SessionAffinityFormat::OpenAi),
        options.cache_retention,
        options.session_id.as_deref(),
        headers,
    )
}

/// Adds ChatGPT Codex SSE session-affinity headers for fully typed options.
/// Codex uses the hyphenated `session-id` spelling from pinned Pi's transport.
pub fn apply_openai_codex_responses_full_headers(
    options: &OpenAiCodexResponsesOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    apply_codex_responses_affinity(
        options.cache_retention,
        options.session_id.as_deref(),
        headers,
    )
}

pub(crate) fn apply_openai_codex_responses_session_affinity_headers(
    options: &SimpleGenerationOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    apply_codex_responses_affinity(
        options.cache_retention.unwrap_or_default(),
        options.session_id.as_deref(),
        headers,
    )
}

fn apply_codex_responses_affinity(
    retention: CacheRetention,
    session_id: Option<&str>,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    let Some(cache_session_id) = openai_codex_responses_transport_session_id(retention, session_id)
    else {
        return Ok(());
    };
    let Some(session_id) = crate::clamp_openai_prompt_cache_key(Some(&cache_session_id)) else {
        return Ok(());
    };
    let value = HeaderValue::from_str(&session_id).map_err(|error| {
        crate::MiddlewareError::new(
            "invalid_header_value",
            format!("session ID cannot be encoded as a header: {error}"),
        )
    })?;
    headers.insert("session-id", value.clone());
    headers.insert("x-client-request-id", value);
    Ok(())
}

/// Selects the typed Codex session key used for request affinity, cached
/// WebSocket continuation, and sticky WebSocket-to-SSE fallback.
///
/// Logical headers deliberately do not participate in this selection.
pub fn openai_codex_responses_transport_session_id(
    retention: CacheRetention,
    session_id: Option<&str>,
) -> Option<String> {
    if retention == CacheRetention::None {
        None
    } else {
        session_id
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned)
    }
}

fn apply_responses_affinity(
    format: SessionAffinityFormat,
    retention: CacheRetention,
    session_id: Option<&str>,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    if retention == CacheRetention::None {
        return Ok(());
    }
    let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) else {
        return Ok(());
    };
    let value = HeaderValue::from_str(session_id).map_err(|error| {
        crate::MiddlewareError::new(
            "invalid_header_value",
            format!("session ID cannot be encoded as a header: {error}"),
        )
    })?;
    let mut insert = |name: &'static str| {
        headers.insert(HeaderName::from_static(name), value.clone());
    };
    match format {
        SessionAffinityFormat::OpenRouter => insert("x-session-id"),
        SessionAffinityFormat::OpenAi => {
            insert("session_id");
            insert("x-client-request-id");
        }
        SessionAffinityFormat::OpenAiNoSession => insert("x-client-request-id"),
    }
    Ok(())
}

/// OpenAI Responses handoff rules for replay recognition and tool-call IDs.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiResponsesHandoff;

impl ApiFamilyHandoff for OpenAiResponsesHandoff {
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool {
        matches!(
            kind.as_str(),
            OPENAI_RESPONSES_REASONING_ITEM_KIND
                | OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND
                | OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND
        )
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        self
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

impl ToolCallIdPolicy for OpenAiResponsesHandoff {
    fn normalize(
        &self,
        original: &ToolCallId,
        source: &ModelFingerprint,
        target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        let allowed = matches!(
            target.provider.as_str(),
            "openai" | "openai-codex" | "opencode"
        );
        let value = original.as_str();
        if !allowed {
            return Ok(ToolCallId::new(normalize_responses_id_part(value)));
        }
        let mut parts = value.split('|');
        let call_id = parts.next().unwrap_or_default();
        let Some(item_id) = parts.next() else {
            return Ok(ToolCallId::new(normalize_responses_id_part(value)));
        };
        let call_id = normalize_responses_id_part(call_id);
        let foreign = source.provider != target.provider || source.api != target.api;
        let mut item_id = if foreign {
            format!("fc_{}", short_hash(item_id))
                .chars()
                .take(64)
                .collect::<String>()
        } else {
            normalize_responses_id_part(item_id)
        };
        if !item_id.starts_with("fc_") {
            item_id = normalize_responses_id_part(&format!("fc_{item_id}"));
        }
        Ok(ToolCallId::new(format!("{call_id}|{item_id}")))
    }
}

fn normalize_responses_id_part(value: &str) -> String {
    let mut normalized = value
        .encode_utf16()
        .map(|unit| {
            u8::try_from(unit)
                .ok()
                .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                .map(char::from)
                .unwrap_or('_')
        })
        .take(64)
        .collect::<String>();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beef_u32;
    let mut h2 = 0x41c6_ce57_u32;
    for unit in value.encode_utf16() {
        h1 = (h1 ^ u32::from(unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(unit)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut reversed = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        reversed.push(if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        });
        value /= 36;
    }
    reversed.into_iter().rev().collect()
}

fn sanitize_responses_text(value: &str) -> String {
    crate::sanitize_utf16_surrogates(&value.encode_utf16().collect::<Vec<_>>())
}
