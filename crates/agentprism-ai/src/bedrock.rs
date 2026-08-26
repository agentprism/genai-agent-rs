//! Amazon Bedrock Converse Stream lowering, replay, and ordered Smithy input.
//!
//! The family owns canonical-context projection and command-input shaping. The
//! provider leaf performs the Smithy serialization boundary, signer-stage
//! header insertion, endpoint routing, and event-stream decoding (Architecture
//! v2 part 2 §1.7, §2.6, §3, §5.1, and §10.2/§10.8).

use crate::{
    ApiFamily, ApiFamilyHandoff, BedrockCompat, BedrockModelConfig, CacheRetention,
    ConstrainedSampling, ConstrainedSamplingConfig, ContentBlock, Context, EncodeContext,
    EncodeError, HandoffError, HandoffReport, JsonSchemaStrictMode, LevelSupport, LoweringError,
    Message, ModelFingerprint, OrderedJsonArray, OrderedJsonObject, OrderedJsonValue,
    ReasoningLevel, ReplayKind, ReplayScope, SecretString, SimpleGenerationOptions,
    SimpleLoweringContext, ThinkingBudgets, ToolCallId, ToolCallIdPolicy, ToolChoice,
    ToolResultContent, TypedModelDescriptor, is_ecmascript_whitespace, make_strict_json_schema,
    plan_thinking_budget,
};
use base64::{
    Engine as _, alphabet,
    engine::{
        DecodePaddingMode,
        general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD},
    },
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use url::Url;

/// Replay kind retaining Bedrock's signed visible reasoning.
pub const BEDROCK_THINKING_SIGNATURE_KIND: &str = "bedrock.converse.thinking-signature";

/// Replay kind retaining Bedrock's opaque redacted reasoning bytes.
pub const BEDROCK_REDACTED_REASONING_KIND: &str = "bedrock.converse.redacted-reasoning";

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

// After the structural checks in `canonicalize_atob_base64`, this engine
// reproduces Infra forgiving-base64 decoding: padding is optional and nonzero
// discarded pad bits are ignored. The AWS serializer then emits the decoded
// bytes as canonical padded base64.
const PI_ATOB: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Marker type for the `bedrock-converse-stream` API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct BedrockConverseStream;

/// Provider display policy for Claude reasoning on Bedrock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockThinkingDisplay {
    /// Request visible summarized reasoning.
    #[default]
    Summarized,
    /// Request an opaque reasoning payload without visible summaries.
    Omitted,
}

/// Native Bedrock tool-selection behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BedrockToolChoice {
    /// Let the model choose.
    Auto,
    /// Require any tool.
    Any,
    /// Omit tools from the request.
    None,
    /// Require one named tool.
    Tool {
        /// Required tool name.
        name: String,
    },
}

impl Serialize for BedrockToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Any => serializer.serialize_str("any"),
            Self::None => serializer.serialize_str("none"),
            Self::Tool { name } => {
                let mut value = serializer.serialize_struct("BedrockToolChoice", 2)?;
                value.serialize_field("type", "tool")?;
                value.serialize_field("name", name)?;
                value.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for BedrockToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Name(String),
            Tool {
                #[serde(rename = "type")]
                kind: String,
                name: String,
            },
        }

        match Wire::deserialize(deserializer)? {
            Wire::Name(name) => match name.as_str() {
                "auto" => Ok(Self::Auto),
                "any" => Ok(Self::Any),
                "none" => Ok(Self::None),
                _ => Err(serde::de::Error::unknown_variant(
                    &name,
                    &["auto", "any", "none"],
                )),
            },
            Wire::Tool { kind, name } if kind == "tool" => Ok(Self::Tool { name }),
            Wire::Tool { kind, .. } => Err(serde::de::Error::unknown_variant(&kind, &["tool"])),
        }
    }
}

/// Fully API-specific Bedrock Converse Stream options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BedrockOptions {
    /// Explicit AWS request region. GovCloud payload shaping depends on this
    /// request-scoped value independently of the catalog endpoint.
    pub region: Option<String>,
    /// Explicit AWS shared-configuration profile selected for this request.
    pub profile: Option<String>,
    /// Bedrock bearer token, which bypasses SigV4 when present.
    pub bearer_token: Option<SecretString>,
    /// Provider-visible response ceiling. `None` preserves full-call omission
    /// for non-Claude models; Claude defaults to the catalog maximum.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Native tool choice.
    pub tool_choice: Option<BedrockToolChoice>,
    /// Provider-neutral reasoning level retained for Bedrock shaping.
    pub reasoning: Option<ReasoningLevel>,
    /// Optional custom token budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Claude interleaved-thinking request preference. Omission means Pi's
    /// `true` default.
    pub interleaved_thinking: Option<bool>,
    /// Claude thinking-display preference.
    pub thinking_display: Option<BedrockThinkingDisplay>,
    /// Explicit prompt-cache retention. Omission permits the provider-scoped
    /// `PI_CACHE_RETENTION=long` compatibility default.
    pub cache_retention: Option<CacheRetention>,
    /// Non-secret provider-environment behavior resolved by the Bedrock leaf.
    /// This is scratch request state and is never persisted in messages.
    #[doc(hidden)]
    pub provider_environment: BedrockProviderEnvironment,
    /// Request tags used by AWS cost allocation.
    pub request_metadata: Option<IndexMap<String, String>>,
}

/// Non-secret Bedrock behavior derived from request-scoped or ambient provider
/// environment values by the provider leaf.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BedrockProviderEnvironment {
    /// `PI_CACHE_RETENTION=long` selected the long default when the request did
    /// not carry an explicit retention option.
    pub long_cache_retention: bool,
    /// `AWS_BEDROCK_FORCE_CACHE=1` enables cache points for otherwise
    /// unidentified application inference profiles.
    pub force_prompt_caching: bool,
}

/// Typed patch accepted by provider-neutral Bedrock simple calls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BedrockSimplePatch {
    /// Replace the provider-neutral `auto`/`none` tool choice with a native one.
    pub tool_choice: Option<BedrockToolChoice>,
    /// Interleaved-thinking override.
    pub interleaved_thinking: Option<bool>,
    /// Thinking-display override.
    pub thinking_display: Option<BedrockThinkingDisplay>,
    /// Request metadata tags.
    pub request_metadata: Option<IndexMap<String, String>>,
}

impl ApiFamily for BedrockConverseStream {
    const API_ID: &'static str = "bedrock-converse-stream";

    type Compat = BedrockCompat;
    type ModelConfig = BedrockModelConfig;
    type FullOptions = BedrockOptions;
    type OptionsPatch = BedrockSimplePatch;
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
        let model = context.model;
        let requested = simple.reasoning;
        let mut maximum = Some(if model.common.limits.context_window == 0 {
            simple
                .max_output_tokens
                .unwrap_or(model.common.limits.max_output_tokens)
                .max(1)
        } else {
            simple
                .max_output_tokens
                .unwrap_or(model.common.limits.max_output_tokens)
                .min(u32::try_from(context.available_context_tokens.max(1)).unwrap_or(u32::MAX))
        });
        let budgets = simple.thinking_budgets.clone();
        let mut lowered_budgets = budgets.clone();
        if let Some(level) = requested
            && is_anthropic_claude(&model.common.model_ref.model, &model.common.display_name)
            && !supports_adaptive_thinking(
                model.common.model_ref.model.as_str(),
                &model.common.display_name,
            )
        {
            let configured = budgets.clone().unwrap_or_default();
            let plan = plan_thinking_budget(
                simple.max_output_tokens,
                model.common.limits.max_output_tokens,
                level,
                &configured,
            )?;
            let max_tokens = if model.common.limits.context_window == 0 {
                plan.max_output_tokens
            } else {
                plan.max_output_tokens
                    .min(u32::try_from(context.available_context_tokens.max(1)).unwrap_or(u32::MAX))
            };
            maximum = Some(max_tokens);
            let budget = plan
                .thinking_budget
                .min(max_tokens.saturating_sub(crate::MIN_ANSWER_TOKENS));
            let effective = clamp_bedrock_level(model, level, simple.reasoning_fallback)?;
            let target = lowered_budgets.get_or_insert_with(ThinkingBudgets::default);
            match effective {
                ReasoningLevel::Minimal => target.minimal = Some(budget),
                ReasoningLevel::Low => target.low = Some(budget),
                ReasoningLevel::Medium => target.medium = Some(budget),
                ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => {
                    target.high = Some(budget);
                }
                ReasoningLevel::Off => {}
            }
        }
        let tool_choice = patch.tool_choice.clone().or_else(|| {
            simple.tool_choice.map(|choice| match choice {
                ToolChoice::Auto => BedrockToolChoice::Auto,
                ToolChoice::None => BedrockToolChoice::None,
            })
        });
        Ok(BedrockOptions {
            region: None,
            profile: None,
            bearer_token: None,
            max_tokens: maximum,
            temperature: simple.temperature,
            tool_choice,
            reasoning: requested,
            thinking_budgets: lowered_budgets,
            interleaved_thinking: patch.interleaved_thinking,
            thinking_display: patch.thinking_display,
            cache_retention: simple.cache_retention,
            provider_environment: BedrockProviderEnvironment::default(),
            request_metadata: patch.request_metadata.clone(),
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_bedrock_converse(context.model, context.context, options)
    }
}

fn clamp_bedrock_level(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    requested: ReasoningLevel,
    fallback: crate::ReasoningFallback,
) -> Result<ReasoningLevel, LoweringError> {
    let supported = |level| match model.config.thinking_levels.get(level) {
        Some(LevelSupport::Unsupported) => false,
        Some(LevelSupport::Disabled | LevelSupport::Value(_)) => true,
        None => !matches!(level, ReasoningLevel::Xhigh | ReasoningLevel::Max),
    };
    if supported(requested) {
        return Ok(requested);
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
        .expect("every reasoning level is listed");
    Ok(LEVELS[index + 1..]
        .iter()
        .copied()
        .chain(LEVELS[..index].iter().rev().copied())
        .find(|level| supported(*level))
        .unwrap_or(ReasoningLevel::Off))
}

/// Encodes the ordered Smithy command input observed by pinned Pi's
/// `onPayload`. `modelId` remains in this logical value; the signer transport
/// consumes it into the request path before serializing the HTTP body.
pub fn encode_bedrock_converse(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    context: &Context,
    options: &BedrockOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let retention = resolved_cache_retention(options);
    let mut request = OrderedJsonObject::new();
    request.insert("modelId", model.common.model_ref.model.as_str());
    request.insert(
        "messages",
        convert_messages(
            model,
            context,
            retention,
            options.provider_environment.force_prompt_caching,
        )?,
    );
    if let Some(system) = build_system(
        model,
        context,
        retention,
        options.provider_environment.force_prompt_caching,
    ) {
        request.insert("system", system);
    }
    let mut inference = OrderedJsonObject::new();
    let maximum = options.max_tokens.or_else(|| {
        is_anthropic_claude(&model.common.model_ref.model, &model.common.display_name)
            .then_some(model.common.limits.max_output_tokens)
    });
    if let Some(maximum) = maximum {
        inference.insert("maxTokens", maximum);
    }
    if let Some(temperature) = options.temperature {
        inference.insert("temperature", temperature);
    }
    request.insert("inferenceConfig", inference);
    if let Some(tools) = convert_tools(model, context, options.tool_choice.as_ref())? {
        request.insert("toolConfig", tools);
    }
    if let Some(fields) = additional_model_fields(model, options)? {
        request.insert("additionalModelRequestFields", fields);
    }
    if let Some(metadata) = &options.request_metadata {
        request.insert(
            "requestMetadata",
            metadata
                .iter()
                .map(|(key, value)| (key.as_str(), OrderedJsonValue::from(value.as_str())))
                .collect::<OrderedJsonObject>(),
        );
    }
    Ok(request)
}

fn build_system(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    context: &Context,
    retention: CacheRetention,
    force_prompt_caching: bool,
) -> Option<OrderedJsonArray> {
    let prompt = context
        .system_prompt
        .as_deref()
        .filter(|value| !value.is_empty())?;
    let mut blocks = OrderedJsonArray::new();
    blocks.push(object([("text", prompt.into())]));
    if retention != CacheRetention::None && supports_prompt_caching(model, force_prompt_caching) {
        blocks.push(cache_point(retention));
    }
    Some(blocks)
}

fn convert_messages(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    context: &Context,
    retention: CacheRetention,
    force_prompt_caching: bool,
) -> Result<OrderedJsonArray, EncodeError> {
    let target = ReplayScope::new(
        model.common.model_ref.provider.clone(),
        BedrockConverseStream::API_ID,
        model.common.model_ref.model.clone(),
        model.common.model_ref.model.clone(),
    );
    let mut messages = Vec::<OrderedJsonValue>::new();
    let mut index = 0;
    while index < context.messages.len() {
        match &context.messages[index] {
            Message::User(user_message) => {
                let mut content = Vec::new();
                for block in &user_message.content {
                    match block {
                        ContentBlock::Text { text, .. } if !is_ecmascript_blank(text) => {
                            content.push(object([("text", text.as_str().into())]).into());
                        }
                        ContentBlock::Image {
                            data, mime_type, ..
                        } => {
                            content
                                .push(object([("image", image(mime_type, data)?.into())]).into());
                        }
                        ContentBlock::Text { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ToolCall { .. } => {}
                    }
                }
                if content.is_empty() {
                    content.push(object([("text", EMPTY_TEXT_PLACEHOLDER.into())]).into());
                }
                messages.push(message("user", content));
            }
            Message::Assistant(message_value) => {
                let mut content = Vec::new();
                for block in &message_value.content {
                    match block {
                        ContentBlock::Text { text, .. } if !is_ecmascript_blank(text) => {
                            content.push(object([("text", text.as_str().into())]).into());
                        }
                        ContentBlock::Thinking {
                            id, text, redacted, ..
                        } if *redacted => {
                            if let Some(bytes) = message_value
                                .replay
                                .complete_item_for_block(
                                    id,
                                    BEDROCK_REDACTED_REASONING_KIND,
                                    &target,
                                )
                                .and_then(crate::ReplayItem::as_bytes)
                                .filter(|bytes| !bytes.is_empty())
                            {
                                content.push(
                                    object([(
                                        "reasoningContent",
                                        object([(
                                            "redactedContent",
                                            STANDARD.encode(bytes).into(),
                                        )])
                                        .into(),
                                    )])
                                    .into(),
                                );
                            }
                        }
                        ContentBlock::Thinking { id, text, .. } if !is_ecmascript_blank(text) => {
                            if is_anthropic_claude(
                                &model.common.model_ref.model,
                                &model.common.display_name,
                            ) {
                                let signature = message_value
                                    .replay
                                    .complete_item_for_block(
                                        id,
                                        BEDROCK_THINKING_SIGNATURE_KIND,
                                        &target,
                                    )
                                    .and_then(crate::ReplayItem::as_utf8)
                                    .filter(|value| !is_ecmascript_blank(value));
                                if let Some(signature) = signature {
                                    content.push(
                                        object([(
                                            "reasoningContent",
                                            object([(
                                                "reasoningText",
                                                object([
                                                    ("text", text.as_str().into()),
                                                    ("signature", signature.into()),
                                                ])
                                                .into(),
                                            )])
                                            .into(),
                                        )])
                                        .into(),
                                    );
                                } else {
                                    content.push(object([("text", text.as_str().into())]).into());
                                }
                            } else {
                                content.push(
                                    object([(
                                        "reasoningContent",
                                        object([(
                                            "reasoningText",
                                            object([("text", text.as_str().into())]).into(),
                                        )])
                                        .into(),
                                    )])
                                    .into(),
                                );
                            }
                        }
                        ContentBlock::ToolCall { call, .. } => {
                            content.push(
                                object([(
                                    "toolUse",
                                    object([
                                        ("toolUseId", call.id.as_str().into()),
                                        ("name", call.name.as_str().into()),
                                        ("input", sanitize_document(&call.arguments).into()),
                                    ])
                                    .into(),
                                )])
                                .into(),
                            );
                        }
                        ContentBlock::Text { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. } => {}
                    }
                }
                if !content.is_empty() {
                    messages.push(message("assistant", content));
                }
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(tool_result)) = context.messages.get(index) {
                    let mut content = Vec::new();
                    for block in &tool_result.content {
                        match block {
                            ToolResultContent::Text { text, .. } if !is_ecmascript_blank(text) => {
                                content.push(object([("text", text.as_str().into())]).into());
                            }
                            ToolResultContent::Image {
                                data, mime_type, ..
                            } => {
                                content.push(
                                    object([("image", image(mime_type, data)?.into())]).into(),
                                );
                            }
                            ToolResultContent::Text { .. } => {}
                        }
                    }
                    if content.is_empty() {
                        content.push(object([("text", EMPTY_TEXT_PLACEHOLDER.into())]).into());
                    }
                    results.push(
                        object([(
                            "toolResult",
                            object([
                                ("toolUseId", tool_result.tool_call_id.as_str().into()),
                                ("content", array(content).into()),
                                (
                                    "status",
                                    if tool_result.is_error {
                                        "error".into()
                                    } else {
                                        "success".into()
                                    },
                                ),
                            ])
                            .into(),
                        )])
                        .into(),
                    );
                    index += 1;
                }
                index -= 1;
                messages.push(message("user", results));
            }
        }
        index += 1;
    }
    if retention != CacheRetention::None
        && supports_prompt_caching(model, force_prompt_caching)
        && let Some(OrderedJsonValue::Object(last)) = messages.last_mut()
        && last
            .get("role")
            .and_then(OrderedJsonValue::as_string)
            .and_then(|value| value.to_utf8().ok())
            .as_deref()
            == Some("user")
        && let Some(OrderedJsonValue::Array(content)) = last.get_mut("content")
    {
        content.push(cache_point(retention));
    }
    Ok(array(messages))
}

fn convert_tools(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    context: &Context,
    choice: Option<&BedrockToolChoice>,
) -> Result<Option<OrderedJsonObject>, EncodeError> {
    if context.tools.is_empty() || matches!(choice, Some(BedrockToolChoice::None)) {
        return Ok(None);
    }
    let supports_strict = model.config.compat.supports_strict_mode.unwrap_or(false);
    let mut tools = Vec::new();
    for tool in &context.tools {
        let strict = match &tool.constrained_sampling {
            Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })) => {
                Some(*strict)
            }
            _ => None,
        };
        if !supports_strict && strict == Some(JsonSchemaStrictMode::Require) {
            return Err(EncodeError::InvalidRequest {
                message: format!(
                    "tool {} requires JSON-schema constrained sampling, but strict tools are unsupported",
                    tool.name
                ),
            });
        }
        let (schema, strict_enabled) = match strict.filter(|_| supports_strict) {
            Some(preference) => match make_strict_json_schema(&tool.parameters) {
                Ok(schema) => (schema, true),
                Err(error) if preference == JsonSchemaStrictMode::Require => {
                    return Err(EncodeError::InvalidRequest { message: error });
                }
                Err(_) => (tool.parameters.clone(), false),
            },
            None => (tool.parameters.clone(), false),
        };
        let mut spec = OrderedJsonObject::new();
        spec.insert("name", tool.name.as_str());
        spec.insert("inputSchema", object([("json", schema.into())]));
        spec.insert("description", tool.description.as_str());
        if strict_enabled {
            spec.insert("strict", true);
        }
        tools.push(object([("toolSpec", spec.into())]).into());
    }
    let mut config = OrderedJsonObject::new();
    config.insert("tools", array(tools));
    let encoded_choice = match choice {
        Some(BedrockToolChoice::Auto) => Some(object([("auto", object([]).into())])),
        Some(BedrockToolChoice::Any) => Some(object([("any", object([]).into())])),
        Some(BedrockToolChoice::Tool { name }) => Some(object([(
            "tool",
            object([("name", name.as_str().into())]).into(),
        )])),
        Some(BedrockToolChoice::None) | None => None,
    };
    if let Some(choice) = encoded_choice {
        config.insert("toolChoice", choice);
    }
    Ok(Some(config))
}

fn additional_model_fields(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    options: &BedrockOptions,
) -> Result<Option<OrderedJsonObject>, EncodeError> {
    let Some(level) = options.reasoning.filter(|_| model.common.reasoning) else {
        return Ok(None);
    };
    if !is_anthropic_claude(&model.common.model_ref.model, &model.common.display_name) {
        return Ok(None);
    }
    let display = (!is_govcloud_bedrock_target(model, options))
        .then(|| options.thinking_display.unwrap_or_default());
    let mut result = OrderedJsonObject::new();
    if supports_adaptive_thinking(
        model.common.model_ref.model.as_str(),
        &model.common.display_name,
    ) {
        let mut thinking = object([("type", "adaptive".into())]);
        if let Some(display) = display {
            thinking.insert("display", display_name(display));
        }
        result.insert("thinking", thinking);
        result.insert(
            "output_config",
            object([("effort", bedrock_effort(model, level).into())]),
        );
    } else {
        let budget_level = match level {
            ReasoningLevel::Xhigh | ReasoningLevel::Max => ReasoningLevel::High,
            value => value,
        };
        let budget = options
            .thinking_budgets
            .clone()
            .unwrap_or_default()
            .budget_for(budget_level)
            .unwrap_or_else(|| default_bedrock_thinking_budget(level));
        let mut thinking = object([("type", "enabled".into()), ("budget_tokens", budget.into())]);
        if let Some(display) = display {
            thinking.insert("display", display_name(display));
        }
        result.insert("thinking", thinking);
        if options.interleaved_thinking.unwrap_or(true) {
            result.insert(
                "anthropic_beta",
                array(vec!["interleaved-thinking-2025-05-14".into()]),
            );
        }
    }
    Ok(Some(result))
}

fn default_bedrock_thinking_budget(level: ReasoningLevel) -> u32 {
    match level {
        ReasoningLevel::Off => 0,
        ReasoningLevel::Minimal => 1_024,
        ReasoningLevel::Low => 2_048,
        ReasoningLevel::Medium => 8_192,
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => 16_384,
    }
}

fn is_govcloud_bedrock_target(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    options: &BedrockOptions,
) -> bool {
    if options
        .region
        .as_deref()
        .is_some_and(|region| region.to_ascii_lowercase().starts_with("us-gov-"))
    {
        return true;
    }
    let model_id = model.common.model_ref.model.as_str().to_ascii_lowercase();
    model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

fn bedrock_effort(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    level: ReasoningLevel,
) -> String {
    if level == ReasoningLevel::Xhigh
        && model_candidates(
            model.common.model_ref.model.as_str(),
            &model.common.display_name,
        )
        .iter()
        .any(|candidate| {
            ["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "fable-5"]
                .iter()
                .any(|needle| candidate.contains(needle))
        })
    {
        return "xhigh".to_owned();
    }
    if let Some(LevelSupport::Value(value)) = model.config.thinking_levels.get(level) {
        return value.clone();
    }
    match level {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => "high",
        ReasoningLevel::Off => "high",
    }
    .to_owned()
}

fn display_name(display: BedrockThinkingDisplay) -> &'static str {
    match display {
        BedrockThinkingDisplay::Summarized => "summarized",
        BedrockThinkingDisplay::Omitted => "omitted",
    }
}

fn image(mime_type: &str, data: &str) -> Result<OrderedJsonObject, EncodeError> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        value => {
            return Err(EncodeError::InvalidRequest {
                message: format!("unknown Bedrock image type: {value}"),
            });
        }
    };
    let encoded = canonicalize_atob_base64(data)?;
    Ok(object([
        ("format", format.into()),
        (
            "source",
            object([("bytes", encoded.as_str().into())]).into(),
        ),
    ]))
}

fn canonicalize_atob_base64(data: &str) -> Result<String, EncodeError> {
    let mut normalized = data
        .bytes()
        .filter(|byte| !matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .collect::<Vec<_>>();
    if normalized.len() % 4 == 0 {
        if normalized.last() == Some(&b'=') {
            normalized.pop();
        }
        if normalized.last() == Some(&b'=') {
            normalized.pop();
        }
    }
    if normalized.len() % 4 == 1
        || normalized
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'+' | b'/'))
    {
        return Err(EncodeError::InvalidRequest {
            message: "invalid Bedrock image base64 data".to_owned(),
        });
    }
    let bytes = PI_ATOB
        .decode(normalized)
        .map_err(|_| EncodeError::InvalidRequest {
            message: "invalid Bedrock image base64 data".to_owned(),
        })?;
    Ok(STANDARD.encode(bytes))
}

fn sanitize_document(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(sanitize_document).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.clone(), sanitize_document(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn cache_point(retention: CacheRetention) -> OrderedJsonObject {
    let mut point = OrderedJsonObject::new();
    point.insert("type", "default");
    if retention == CacheRetention::Long {
        point.insert("ttl", "1h");
    }
    object([("cachePoint", point.into())])
}

fn resolved_cache_retention(options: &BedrockOptions) -> CacheRetention {
    options.cache_retention.unwrap_or({
        if options.provider_environment.long_cache_retention {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn supports_prompt_caching(
    model: &TypedModelDescriptor<BedrockConverseStream>,
    force_prompt_caching: bool,
) -> bool {
    let candidates = model_candidates(
        model.common.model_ref.model.as_str(),
        &model.common.display_name,
    );
    let has_claude_reference = candidates
        .iter()
        .any(|candidate| candidate.contains("claude"));
    if !has_claude_reference {
        return force_prompt_caching;
    }
    candidates.iter().any(|candidate| {
        candidate.contains("fable-5")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("-4-")
            || candidate.contains("claude-3-7-sonnet")
            || candidate.contains("claude-3-5-haiku")
    })
}

fn supports_adaptive_thinking(model_id: &str, name: &str) -> bool {
    model_candidates(model_id, name).iter().any(|candidate| {
        [
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "opus-5",
            "sonnet-4-6",
            "sonnet-5",
            "fable-5",
        ]
        .iter()
        .any(|needle| candidate.contains(needle))
    })
}

fn is_anthropic_claude(model_id: &crate::ModelId, name: &str) -> bool {
    let id = model_id.as_str().to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn model_candidates(model_id: &str, name: &str) -> Vec<String> {
    [model_id, name]
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let mut normalized = String::with_capacity(lower.len());
            let mut in_separator_run = false;
            for character in lower.chars() {
                if is_ecmascript_whitespace(character) || matches!(character, '_' | '.' | ':') {
                    if !in_separator_run {
                        normalized.push('-');
                        in_separator_run = true;
                    }
                } else {
                    normalized.push(character);
                    in_separator_run = false;
                }
            }
            [lower, normalized]
        })
        .collect()
}

fn is_ecmascript_blank(value: &str) -> bool {
    value.chars().all(is_ecmascript_whitespace)
}

fn message(role: &str, content: Vec<OrderedJsonValue>) -> OrderedJsonValue {
    object([("role", role.into()), ("content", array(content).into())]).into()
}

fn object<const N: usize>(entries: [(&str, OrderedJsonValue); N]) -> OrderedJsonObject {
    entries.into_iter().collect()
}

fn array(values: Vec<OrderedJsonValue>) -> OrderedJsonArray {
    values.into_iter().collect()
}

/// Bedrock-compatible tool-call identifier normalization.
#[derive(Clone, Copy, Debug, Default)]
pub struct BedrockToolCallIdPolicy;

impl ToolCallIdPolicy for BedrockToolCallIdPolicy {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        // Pi's non-`u` JavaScript regex visits UTF-16 code units, not Unicode
        // scalar values. Replacing before `.slice(0, 64)` therefore turns one
        // astral scalar into two underscores and counts both toward the limit.
        let normalized = original
            .as_str()
            .encode_utf16()
            .map(|code_unit| {
                if matches!(
                    code_unit,
                    0x30..=0x39 | 0x41..=0x5a | 0x5f | 0x61..=0x7a | 0x2d
                ) {
                    char::from_u32(u32::from(code_unit)).unwrap_or('_')
                } else {
                    '_'
                }
            })
            .take(64)
            .collect::<String>();
        Ok(ToolCallId::new(normalized))
    }
}

/// Bedrock replay and handoff hooks.
#[derive(Clone, Copy, Debug, Default)]
pub struct BedrockHandoff;

impl ApiFamilyHandoff for BedrockHandoff {
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool {
        matches!(
            kind.as_str(),
            BEDROCK_THINKING_SIGNATURE_KIND | BEDROCK_REDACTED_REASONING_KIND
        )
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        &BedrockToolCallIdPolicy
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}
