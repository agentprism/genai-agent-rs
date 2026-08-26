//! Google Gemini Developer API and Vertex lowering, replay, and wire shaping.
//!
//! Both transports consume the same Google content contract. Authentication,
//! endpoint construction, and stream decoding remain in the `pi-ai-google`
//! provider leaf (Architecture v2 part 2 §1.8, §3, §5.1, and §10.8).

use crate::{
    ApiFamily, ApiFamilyHandoff, ContentBlock, Context, EncodeContext, EncodeError,
    GoogleModelConfig, HandoffError, HandoffReport, JsonSchemaStrictMode, LevelSupport,
    LoweringError, Message, Modality, ModelFingerprint, OrderedJsonArray, OrderedJsonObject,
    OrderedJsonValue, ReasoningLevel, ReplayKind, ReplayScope, ReplayTarget,
    SimpleGenerationOptions, SimpleLoweringContext, ToolCallId, ToolCallIdPolicy, ToolChoice,
    ToolResultContent, TypedModelDescriptor, make_strict_json_schema,
};
use serde::{Deserialize, Serialize};
use url::Url;

/// Replay kind retaining Google's `thoughtSignature` part property.
pub const GOOGLE_THOUGHT_SIGNATURE_KIND: &str = "google.genai.thought-signature";

/// Marker for the Gemini Developer API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoogleGenerativeAi;

/// Marker for the Vertex Gemini API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoogleVertex;

/// Google-native thinking level accepted by Gemini 3 level-based models.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GoogleThinkingLevel {
    /// Let the provider select its default.
    ThinkingLevelUnspecified,
    /// Minimum supported reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
}

impl GoogleThinkingLevel {
    fn wire_name(self) -> &'static str {
        match self {
            Self::ThinkingLevelUnspecified => "THINKING_LEVEL_UNSPECIFIED",
            Self::Minimal => "MINIMAL",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

/// Full Google thinking option object.
///
/// The optional fields intentionally remain independent: pinned Pi accepts an
/// enabled object with neither selector and, when both are present, gives the
/// level precedence over the token budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GoogleThinkingOptions {
    /// Whether visible thought summaries are requested.
    pub enabled: bool,
    /// Provider thinking-token budget. `-1` is Google's dynamic sentinel.
    pub budget_tokens: Option<i32>,
    /// Provider-native thinking level.
    pub level: Option<GoogleThinkingLevel>,
}

/// Google function-calling selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoogleToolChoice {
    /// Let the model choose.
    Auto,
    /// Disable function calls.
    None,
    /// Require a function call.
    Any,
}

/// Fully API-specific options for the Gemini Developer API.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GoogleOptions {
    /// Provider output-token cap; `None` preserves full-options omission.
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Thinking configuration.
    pub thinking: Option<GoogleThinkingOptions>,
    /// Explicit function-calling selection.
    pub tool_choice: Option<GoogleToolChoice>,
}

/// Fully API-specific options for Google Vertex AI.
///
/// `project` and `location` are request-scoped and take precedence over the
/// corresponding credential or ambient environment values, matching pinned
/// Pi's `GoogleVertexOptions` contract.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GoogleVertexOptions {
    /// Provider output-token cap; `None` preserves full-options omission.
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Optional full thinking object.
    pub thinking: Option<GoogleThinkingOptions>,
    /// Explicit function-calling selection.
    pub tool_choice: Option<GoogleToolChoice>,
    /// Per-call Google Cloud project override.
    pub project: Option<String>,
    /// Per-call Google Cloud location override.
    pub location: Option<String>,
}

/// Typed simple-call patch shared by the two Google families.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GoogleSimplePatch {
    /// Override the simple tool choice, including Google's `ANY` mode.
    pub tool_choice: Option<GoogleToolChoice>,
}

/// Google compatibility has no endpoint-detected switches at the pinned Pi
/// commit. The empty, extensible type keeps the API-family seam typed.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleCompat {}

impl ApiFamily for GoogleGenerativeAi {
    const API_ID: &'static str = "google-generative-ai";

    type Compat = GoogleCompat;
    type ModelConfig = GoogleModelConfig;
    type FullOptions = GoogleOptions;
    type OptionsPatch = GoogleSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(model_overrides.clone())
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        let lowered = lower_google_simple(
            context.model,
            context.available_context_tokens,
            simple,
            patch,
        )?;
        Ok(GoogleOptions {
            max_output_tokens: lowered.max_output_tokens,
            temperature: lowered.temperature,
            thinking: lowered.thinking,
            tool_choice: lowered.tool_choice,
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_google(context.model, context.context, options, Self::API_ID)
    }
}

impl ApiFamily for GoogleVertex {
    const API_ID: &'static str = "google-vertex";

    type Compat = GoogleCompat;
    type ModelConfig = GoogleModelConfig;
    type FullOptions = GoogleVertexOptions;
    type OptionsPatch = GoogleSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(model_overrides.clone())
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        let lowered = lower_google_simple(
            context.model,
            context.available_context_tokens,
            simple,
            patch,
        )?;
        Ok(GoogleVertexOptions {
            max_output_tokens: lowered.max_output_tokens,
            temperature: lowered.temperature,
            thinking: lowered.thinking,
            tool_choice: lowered.tool_choice,
            project: None,
            location: None,
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_google(context.model, context.context, options, Self::API_ID)
    }
}

struct LoweredGoogleOptions {
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    thinking: Option<GoogleThinkingOptions>,
    tool_choice: Option<GoogleToolChoice>,
}

trait GoogleOptionView {
    fn max_output_tokens(&self) -> Option<u32>;
    fn temperature(&self) -> Option<f32>;
    fn thinking(&self) -> Option<GoogleThinkingOptions>;
    fn tool_choice(&self) -> Option<GoogleToolChoice>;
}

macro_rules! impl_google_option_view {
    ($options:ty) => {
        impl GoogleOptionView for $options {
            fn max_output_tokens(&self) -> Option<u32> {
                self.max_output_tokens
            }

            fn temperature(&self) -> Option<f32> {
                self.temperature
            }

            fn thinking(&self) -> Option<GoogleThinkingOptions> {
                self.thinking
            }

            fn tool_choice(&self) -> Option<GoogleToolChoice> {
                self.tool_choice
            }
        }
    };
}

impl_google_option_view!(GoogleOptions);
impl_google_option_view!(GoogleVertexOptions);

fn lower_google_simple<A: ApiFamily<ModelConfig = GoogleModelConfig>>(
    model: &TypedModelDescriptor<A>,
    available_context_tokens: u64,
    simple: &SimpleGenerationOptions,
    patch: &GoogleSimplePatch,
) -> Result<LoweredGoogleOptions, LoweringError> {
    let requested = simple
        .max_output_tokens
        .unwrap_or(model.common.limits.max_output_tokens);
    let max_output_tokens = if model.common.limits.context_window == 0 {
        requested.max(1)
    } else {
        requested.min(u32::try_from(available_context_tokens.max(1)).unwrap_or(u32::MAX))
    };
    let thinking = match simple.reasoning {
        _ if !model.common.reasoning => None,
        None => Some(GoogleThinkingOptions {
            enabled: false,
            budget_tokens: None,
            level: None,
        }),
        Some(requested) => resolve_google_thinking(
            model,
            requested,
            simple.reasoning_fallback,
            simple.thinking_budgets.as_ref(),
        )?
        .into(),
    };
    Ok(LoweredGoogleOptions {
        max_output_tokens: Some(max_output_tokens),
        temperature: simple.temperature,
        thinking,
        tool_choice: patch
            .tool_choice
            .or(simple.tool_choice.map(|choice| match choice {
                ToolChoice::Auto => GoogleToolChoice::Auto,
                ToolChoice::None => GoogleToolChoice::None,
            })),
    })
}

fn resolve_google_thinking<A: ApiFamily<ModelConfig = GoogleModelConfig>>(
    model: &TypedModelDescriptor<A>,
    requested: ReasoningLevel,
    fallback: crate::ReasoningFallback,
    budgets: Option<&crate::ThinkingBudgets>,
) -> Result<GoogleThinkingOptions, LoweringError> {
    let (effective, support) =
        clamp_google_reasoning_level(&model.config.thinking_levels, requested, fallback)?;
    if matches!(support, Some(LevelSupport::Disabled)) {
        return Ok(GoogleThinkingOptions {
            enabled: false,
            budget_tokens: None,
            level: None,
        });
    }
    let mapped = match support {
        Some(LevelSupport::Value(ref mapped)) => mapped.as_str(),
        Some(LevelSupport::Unsupported) => unreachable!("resolve removes unsupported levels"),
        Some(LevelSupport::Disabled) => unreachable!("disabled handled above"),
        None => reasoning_name(effective),
    };
    // Pinned Pi tests the logical level before consulting the model map.
    // Therefore logical `off` means Google's `high`, while a non-off level
    // whose provider mapping is the string `off` is invalid.
    let resolved = if effective == ReasoningLevel::Off {
        ReasoningLevel::High
    } else {
        match mapped.to_ascii_lowercase().as_str() {
            "minimal" => ReasoningLevel::Minimal,
            "low" => ReasoningLevel::Low,
            "medium" => ReasoningLevel::Medium,
            "high" => ReasoningLevel::High,
            _ => {
                return Err(LoweringError::InvalidConfiguration {
                    message: format!(
                        "unsupported Google thinking level mapping for {}/{}: {} -> {mapped}",
                        model.common.model_ref.provider,
                        model.common.model_ref.model,
                        reasoning_name(requested)
                    ),
                });
            }
        }
    };
    let model_id = model.common.model_ref.model.as_str();
    if is_gemini_3_pro(model_id) {
        return Ok(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(match resolved {
                ReasoningLevel::Minimal | ReasoningLevel::Low => GoogleThinkingLevel::Low,
                _ => GoogleThinkingLevel::High,
            }),
        });
    }
    if is_gemini_3_flash(model_id) {
        return Ok(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(level_value(resolved)),
        });
    }
    if A::API_ID == GoogleGenerativeAi::API_ID && is_gemma_4(model_id) {
        return Ok(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(match resolved {
                ReasoningLevel::Minimal | ReasoningLevel::Low => GoogleThinkingLevel::Minimal,
                _ => GoogleThinkingLevel::High,
            }),
        });
    }
    let configured = budgets.and_then(|budgets| match resolved {
        ReasoningLevel::Minimal => budgets.minimal,
        ReasoningLevel::Low => budgets.low,
        ReasoningLevel::Medium => budgets.medium,
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => budgets.high,
        ReasoningLevel::Off => Some(0),
    });
    Ok(GoogleThinkingOptions {
        enabled: true,
        budget_tokens: Some(configured.map_or_else(
            || default_google_budget(A::API_ID, model_id, resolved),
            |budget| i32::try_from(budget).unwrap_or(i32::MAX),
        )),
        level: None,
    })
}

fn clamp_google_reasoning_level(
    levels: &crate::ThinkingLevelMap<String>,
    requested: ReasoningLevel,
    fallback: crate::ReasoningFallback,
) -> Result<(ReasoningLevel, Option<LevelSupport<String>>), LoweringError> {
    let supported = |level| match levels.get(level) {
        Some(LevelSupport::Unsupported) => false,
        Some(LevelSupport::Disabled | LevelSupport::Value(_)) => true,
        None => matches!(
            level,
            ReasoningLevel::Off
                | ReasoningLevel::Minimal
                | ReasoningLevel::Low
                | ReasoningLevel::Medium
                | ReasoningLevel::High
        ),
    };
    if supported(requested) {
        return Ok((requested, levels.get(requested).cloned()));
    }
    if matches!(fallback, crate::ReasoningFallback::Strict) {
        return Err(LoweringError::UnsupportedReasoningLevel { requested });
    }
    const LEVELS: [ReasoningLevel; 7] = [
        ReasoningLevel::Off,
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::Xhigh,
        ReasoningLevel::Max,
    ];
    let index = LEVELS
        .iter()
        .position(|level| *level == requested)
        .expect("all reasoning levels are listed");
    let effective = LEVELS[index + 1..]
        .iter()
        .copied()
        .chain(LEVELS[..index].iter().rev().copied())
        .find(|level| supported(*level));
    let Some(effective) = effective else {
        return Ok((ReasoningLevel::Off, Some(LevelSupport::Disabled)));
    };
    Ok((effective, levels.get(effective).cloned()))
}

fn default_google_budget(api_id: &str, model_id: &str, level: ReasoningLevel) -> i32 {
    let (minimal, low, medium, high) = if model_id.contains("2.5-pro") {
        (128, 2_048, 8_192, 32_768)
    } else if api_id == GoogleGenerativeAi::API_ID && model_id.contains("2.5-flash-lite") {
        (512, 2_048, 8_192, 24_576)
    } else if model_id.contains("2.5-flash") {
        (128, 2_048, 8_192, 24_576)
    } else {
        return -1;
    };
    match level {
        ReasoningLevel::Minimal => minimal,
        ReasoningLevel::Low => low,
        ReasoningLevel::Medium => medium,
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => high,
        ReasoningLevel::Off => 0,
    }
}

fn level_value(level: ReasoningLevel) -> GoogleThinkingLevel {
    match level {
        ReasoningLevel::Minimal => GoogleThinkingLevel::Minimal,
        ReasoningLevel::Low => GoogleThinkingLevel::Low,
        ReasoningLevel::Medium => GoogleThinkingLevel::Medium,
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => {
            GoogleThinkingLevel::High
        }
        ReasoningLevel::Off => GoogleThinkingLevel::ThinkingLevelUnspecified,
    }
}

fn reasoning_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "off",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::Xhigh => "xhigh",
        ReasoningLevel::Max => "max",
    }
}

/// Encodes either Google family with the insertion order emitted by pinned
/// Pi's `@google/genai` SDK.
fn encode_google<A, O>(
    model: &TypedModelDescriptor<A>,
    context: &Context,
    options: &O,
    api_id: &str,
) -> Result<OrderedJsonObject, EncodeError>
where
    A: ApiFamily<ModelConfig = GoogleModelConfig>,
    O: GoogleOptionView,
{
    let mut request = OrderedJsonObject::new();
    request.insert("contents", convert_messages(model, context, api_id)?);
    if let Some(system) = context
        .system_prompt
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        request.insert(
            "systemInstruction",
            object([
                ("parts", array(vec![object([("text", system.into())])])),
                ("role", "user".into()),
            ]),
        );
    }
    let (tools, use_validated_mode) = convert_tools(
        &context.tools,
        false,
        supports_google_strict_tool_sampling(model.common.model_ref.model.as_str()),
        api_id,
    )?;
    if let Some(tools) = tools {
        request.insert("tools", tools);
    }
    let calling_mode = match options.tool_choice() {
        Some(GoogleToolChoice::None) => Some("NONE"),
        Some(GoogleToolChoice::Any) => Some("ANY"),
        Some(GoogleToolChoice::Auto) => Some("AUTO"),
        None if use_validated_mode => Some("VALIDATED"),
        None => None,
    };
    if let Some(mode) = calling_mode {
        request.insert(
            "toolConfig",
            object([("functionCallingConfig", object([("mode", mode.into())]))]),
        );
    }
    let mut generation = OrderedJsonObject::new();
    if let Some(temperature) = options.temperature() {
        generation.insert("temperature", temperature);
    }
    if let Some(maximum) = options.max_output_tokens() {
        generation.insert("maxOutputTokens", maximum);
    }
    if model.common.reasoning
        && let Some(options) = options.thinking()
    {
        let mut thinking = OrderedJsonObject::new();
        if options.enabled {
            thinking.insert("includeThoughts", true);
            if let Some(level) = options.level {
                thinking.insert("thinkingLevel", level.wire_name());
            } else if let Some(budget) = options.budget_tokens {
                thinking.insert("thinkingBudget", budget);
            }
        } else if let Some(level) =
            disabled_thinking_level(api_id, model.common.model_ref.model.as_str())
        {
            thinking.insert("thinkingLevel", level.wire_name());
        } else {
            thinking.insert("thinkingBudget", 0_i32);
        }
        generation.insert("thinkingConfig", thinking);
    }
    request.insert("generationConfig", generation);
    Ok(request)
}

fn disabled_thinking_level(api_id: &str, model_id: &str) -> Option<GoogleThinkingLevel> {
    if is_gemini_3_pro(model_id) {
        Some(GoogleThinkingLevel::Low)
    } else if is_gemini_3_flash(model_id)
        || (api_id == GoogleGenerativeAi::API_ID && is_gemma_4(model_id))
    {
        Some(GoogleThinkingLevel::Minimal)
    } else {
        None
    }
}

fn convert_messages<A: ApiFamily<ModelConfig = GoogleModelConfig>>(
    model: &TypedModelDescriptor<A>,
    context: &Context,
    api_id: &str,
) -> Result<OrderedJsonArray, EncodeError> {
    let target = ReplayScope::new(
        model.common.model_ref.provider.clone(),
        api_id,
        model.common.model_ref.model.clone(),
        model.common.model_ref.model.clone(),
    );
    let model_id = model.common.model_ref.model.as_str();
    let supports_images = model.common.modalities.input.contains(&Modality::Image);
    let multimodal_responses = supports_multimodal_function_response(model_id);
    let mut contents = Vec::<OrderedJsonValue>::new();
    for message in &context.messages {
        match message {
            Message::User(message) => {
                let mut parts = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            parts.push(object([("text", text.as_str().into())]));
                        }
                        ContentBlock::Image {
                            data, mime_type, ..
                        } => {
                            // The SDK's user-content normalizer emits `data`
                            // before `mimeType`; function-response image parts
                            // below preserve their independent order.
                            let inline_data = if api_id == GoogleVertex::API_ID {
                                object([
                                    ("mimeType", mime_type.as_str().into()),
                                    ("data", data.as_str().into()),
                                ])
                            } else {
                                object([
                                    ("data", data.as_str().into()),
                                    ("mimeType", mime_type.as_str().into()),
                                ])
                            };
                            parts.push(object([("inlineData", inline_data)]));
                        }
                        ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(content("user", parts));
                }
            }
            Message::Assistant(message) => {
                let mut parts = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { id, text } => {
                            let signature = thought_signature(
                                message,
                                &ReplayTarget::ContentBlock(id.clone()),
                                &target,
                            );
                            if text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = OrderedJsonObject::new();
                            part.insert("text", text.as_str());
                            if let Some(signature) = signature {
                                part.insert("thoughtSignature", signature);
                            }
                            parts.push(part.into());
                        }
                        ContentBlock::Thinking { id, text, .. } => {
                            let signature = thought_signature(
                                message,
                                &ReplayTarget::ContentBlock(id.clone()),
                                &target,
                            );
                            if text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = OrderedJsonObject::new();
                            part.insert("text", text.as_str());
                            part.insert("thought", true);
                            if let Some(signature) = signature {
                                part.insert("thoughtSignature", signature);
                            }
                            parts.push(part.into());
                        }
                        ContentBlock::ToolCall { call, .. } => {
                            let mut function = OrderedJsonObject::new();
                            let arguments = if call.arguments.is_null() {
                                object([])
                            } else {
                                call.arguments.clone().into()
                            };
                            if api_id == GoogleVertex::API_ID {
                                function.insert("name", call.name.as_str());
                                function.insert("args", arguments);
                                if requires_google_tool_call_id(model_id) {
                                    function.insert("id", call.id.as_str());
                                }
                            } else {
                                if requires_google_tool_call_id(model_id) {
                                    function.insert("id", call.id.as_str());
                                }
                                function.insert("args", arguments);
                                function.insert("name", call.name.as_str());
                            }
                            let mut part = OrderedJsonObject::new();
                            part.insert("functionCall", function);
                            if let Some(signature) = thought_signature(
                                message,
                                &ReplayTarget::ToolCall(call.id.clone()),
                                &target,
                            ) {
                                part.insert("thoughtSignature", signature);
                            }
                            parts.push(part.into());
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(content("model", parts));
                }
            }
            Message::ToolResult(message) => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text { text, .. } => Some(text.as_str()),
                        ToolResultContent::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images = if supports_images {
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ToolResultContent::Image {
                                data, mime_type, ..
                            } => Some(object([(
                                "inlineData",
                                object([
                                    ("mimeType", mime_type.as_str().into()),
                                    ("data", data.as_str().into()),
                                ]),
                            )])),
                            ToolResultContent::Text { .. } => None,
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let response_text = if !text.is_empty() {
                    text
                } else if !images.is_empty() {
                    "(see attached image)".to_owned()
                } else {
                    String::new()
                };
                let mut function_response = OrderedJsonObject::new();
                function_response.insert("name", message.tool_name.as_str());
                function_response.insert(
                    "response",
                    object([(
                        if message.is_error { "error" } else { "output" },
                        response_text.as_str().into(),
                    )]),
                );
                if !images.is_empty() && multimodal_responses {
                    function_response.insert("parts", array(images.clone()));
                }
                if requires_google_tool_call_id(model_id) {
                    function_response.insert("id", message.tool_call_id.as_str());
                }
                let response_part = object([("functionResponse", function_response.into())]);
                if let Some(OrderedJsonValue::Object(last)) = contents.last_mut()
                    && last.get("role").and_then(string_value).as_deref() == Some("user")
                    && last
                        .get("parts")
                        .and_then(OrderedJsonValue::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.as_object()
                                    .is_some_and(|part| part.get("functionResponse").is_some())
                            })
                        })
                    && let Some(OrderedJsonValue::Array(parts)) = last.get_mut("parts")
                {
                    parts.push(response_part);
                } else {
                    contents.push(content("user", vec![response_part]));
                }
                if !images.is_empty() && !multimodal_responses {
                    let mut parts = vec![object([("text", "Tool result image:".into())])];
                    parts.extend(images);
                    contents.push(content("user", parts));
                }
            }
        }
    }
    Ok(contents.into_iter().collect())
}

fn thought_signature<'a>(
    message: &'a crate::AssistantMessage,
    replay_target: &ReplayTarget,
    target: &ReplayScope,
) -> Option<&'a str> {
    message
        .replay
        .complete_item(replay_target, GOOGLE_THOUGHT_SIGNATURE_KIND, target)
        .and_then(crate::ReplayItem::as_utf8)
        .filter(|signature| is_valid_thought_signature(signature))
}

fn is_valid_thought_signature(signature: &str) -> bool {
    let padding = signature
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    let payload_length = signature.len().saturating_sub(padding);
    payload_length > 0
        && signature.len().is_multiple_of(4)
        && padding <= 2
        && signature.as_bytes()[..payload_length].iter().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/'
            )
        })
}

fn content(role: &str, parts: Vec<OrderedJsonValue>) -> OrderedJsonValue {
    object([("parts", array(parts)), ("role", role.into())])
}

fn convert_tools(
    tools: &[crate::ToolSpec],
    use_parameters: bool,
    supports_strict: bool,
    api_id: &str,
) -> Result<(Option<OrderedJsonValue>, bool), EncodeError> {
    if tools.is_empty() {
        return Ok((None, false));
    }
    let mut declarations = Vec::new();
    let mut use_validated = false;
    for tool in tools {
        let requested = match tool.constrained_sampling {
            Some(crate::ConstrainedSampling::Config(
                crate::ConstrainedSamplingConfig::JsonSchema { strict },
            )) => Some(strict),
            _ => None,
        };
        let schema = match requested {
            Some(JsonSchemaStrictMode::Require) if !supports_strict => {
                return Err(EncodeError::InvalidRequest {
                    message: format!(
                        "tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported",
                        tool.name
                    ),
                });
            }
            Some(JsonSchemaStrictMode::Prefer) if !supports_strict => tool.parameters.clone(),
            Some(preference) => match make_strict_json_schema(&tool.parameters) {
                Ok(schema) => {
                    use_validated = true;
                    schema
                }
                Err(_) if preference == JsonSchemaStrictMode::Prefer => tool.parameters.clone(),
                Err(message) => {
                    return Err(EncodeError::InvalidRequest {
                        message: format!(
                            "tool \"{}\" requires JSON-schema constrained sampling, but {message}",
                            tool.name
                        ),
                    });
                }
            },
            None => tool.parameters.clone(),
        };
        let parameter_name = if use_parameters {
            "parameters"
        } else {
            "parametersJsonSchema"
        };
        let schema = if use_parameters {
            sanitize_google_open_api_schema(&schema)
        } else {
            schema
        };
        declarations.push(if api_id == GoogleVertex::API_ID {
            object([
                ("description", tool.description.as_str().into()),
                ("name", tool.name.as_str().into()),
                (parameter_name, schema.into()),
            ])
        } else {
            object([
                ("name", tool.name.as_str().into()),
                ("description", tool.description.as_str().into()),
                (parameter_name, schema.into()),
            ])
        });
    }
    Ok((
        Some(array(vec![object([(
            "functionDeclarations",
            array(declarations),
        )])])),
        use_validated,
    ))
}

/// Converts Google function declarations using pinned Pi's shared schema
/// modes. OpenAPI `parameters` recursively strips schema-document metadata;
/// `parametersJsonSchema` preserves the original JSON Schema vocabulary.
pub fn convert_google_tools(
    tools: &[crate::ToolSpec],
    use_parameters: bool,
    supports_strict: bool,
) -> Result<Option<OrderedJsonValue>, EncodeError> {
    convert_tools(
        tools,
        use_parameters,
        supports_strict,
        GoogleGenerativeAi::API_ID,
    )
    .map(|(tools, _)| tools)
}

fn sanitize_google_open_api_schema(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "$schema"
                            | "$id"
                            | "$anchor"
                            | "$dynamicAnchor"
                            | "$vocabulary"
                            | "$comment"
                            | "$defs"
                            | "definitions"
                    )
                })
                .map(|(key, value)| (key.clone(), sanitize_google_open_api_schema(value)))
                .collect(),
        ),
        // Pinned Pi's helper only descends through object-valued properties;
        // arrays are preserved as-is.
        value => value.clone(),
    }
}

/// Whether the model expects Gemini's provider tool-call ID field.
pub fn requires_google_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

/// Whether Gemini supports strict JSON-schema function calling.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|major| major >= 3)
}

fn gemini_major_version(model_id: &str) -> Option<u32> {
    let lower = model_id.to_ascii_lowercase();
    let version = lower
        .strip_prefix("gemini-live-")
        .or_else(|| lower.strip_prefix("gemini-"))?;
    let digits = version
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn is_gemini_3_pro(model_id: &str) -> bool {
    is_gemini_3_variant(model_id, "pro")
}

fn is_gemini_3_flash(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    is_gemini_3_variant(&id, "flash")
        || matches!(
            id.as_str(),
            "gemini-flash-latest" | "gemini-flash-lite-latest"
        )
}

fn is_gemini_3_variant(model_id: &str, variant: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.match_indices("gemini-3")
        .any(|(start, _)| is_gemini_3_variant_suffix(&id[start + "gemini-3".len()..], variant))
}

fn is_gemini_3_variant_suffix(suffix: &str, variant: &str) -> bool {
    if suffix.starts_with(&format!("-{variant}")) {
        return true;
    }
    let Some(versioned) = suffix.strip_prefix('.') else {
        return false;
    };
    let digits = versioned.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && versioned[digits..].starts_with(&format!("-{variant}"))
}

fn is_gemma_4(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("gemma-4") || id.contains("gemma4")
}

fn string_value(value: &OrderedJsonValue) -> Option<String> {
    value.as_string()?.to_utf8().ok()
}

fn object<const N: usize>(entries: [(&str, OrderedJsonValue); N]) -> OrderedJsonValue {
    OrderedJsonValue::Object(entries.into_iter().collect())
}

fn array(values: Vec<OrderedJsonValue>) -> OrderedJsonValue {
    OrderedJsonValue::Array(values.into_iter().collect())
}

/// Google tool-call identifier normalization from pinned Pi. IDs are left
/// untouched for older Gemini models and normalized as UTF-16 code units for
/// Gemini 3, Claude, and gpt-oss targets.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoogleToolCallIdPolicy;

impl ToolCallIdPolicy for GoogleToolCallIdPolicy {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        if !requires_google_tool_call_id(target.model.as_str()) {
            return Ok(original.clone());
        }
        let units = original
            .as_str()
            .encode_utf16()
            .map(|unit| {
                if unit <= 0x7f
                    && char::from_u32(u32::from(unit)).is_some_and(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                    })
                {
                    unit
                } else {
                    u16::from(b'_')
                }
            })
            .take(64)
            .collect::<Vec<_>>();
        Ok(ToolCallId::new(
            String::from_utf16(&units).expect("Google ID normalization emits ASCII"),
        ))
    }
}

/// Google replay and handoff hooks shared by Developer API and Vertex.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoogleHandoff;

impl ApiFamilyHandoff for GoogleHandoff {
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool {
        kind.as_str() == GOOGLE_THOUGHT_SIGNATURE_KIND
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        &GoogleToolCallIdPolicy
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}
