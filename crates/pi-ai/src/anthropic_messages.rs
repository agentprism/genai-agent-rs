//! Anthropic Messages lowering, replay encoding, and byte-stable wire shape.
//!
//! The API-family contract lives in `pi-ai`; HTTP authentication and SSE
//! decoding live in the `pi-ai-anthropic` provider leaf. This realizes
//! Architecture v2 part 2 §1.4, §3.5, §5.1, and §10.2/§10.5/§10.8.

use crate::{
    AnthropicEffort, AnthropicMessagesCompat, AnthropicMessagesModelConfig, AnthropicThinkingValue,
    ApiFamily, ApiFamilyHandoff, CacheRetention, ContentBlock, Context, EncodeContext, EncodeError,
    HandoffError, HandoffReport, LevelSupport, LoweringError, ModelFingerprint, OrderedJsonArray,
    OrderedJsonObject, OrderedJsonValue, ReasoningLevel, ReplayKind, ReplayScope,
    SimpleGenerationOptions, SimpleLoweringContext, ToolCallId, ToolCallIdPolicy, ToolChoice,
    ToolResultContent, TypedModelDescriptor, make_strict_json_schema, plan_thinking_budget,
};
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use url::Url;

/// Replay kind retaining an Anthropic visible-thinking signature.
pub const ANTHROPIC_THINKING_SIGNATURE_KIND: &str = "anthropic.messages.thinking-signature";

/// Replay kind retaining an Anthropic redacted-thinking payload.
pub const ANTHROPIC_REDACTED_THINKING_KIND: &str = "anthropic.messages.redacted-thinking";

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Marker type for the `anthropic-messages` API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicMessages;

/// Provider display policy for Anthropic extended thinking.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicThinkingDisplay {
    /// Ask the provider for summarized visible reasoning.
    #[default]
    Summarized,
    /// Ask the provider to omit visible reasoning.
    Omitted,
}

/// Fully lowered Anthropic reasoning mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnthropicThinking {
    /// Full-options callers did not request a `thinking` wire member.
    Omitted,
    /// Reasoning is disabled.
    Disabled,
    /// The model controls its own thinking budget.
    Adaptive {
        /// Optional provider-native adaptive effort.
        effort: Option<AnthropicEffort>,
    },
    /// The request allocates a fixed thinking-token budget.
    Budget {
        /// Tokens reserved for thinking within `max_tokens`.
        budget_tokens: u32,
    },
}

/// Native Anthropic tool-choice mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnthropicToolChoice {
    /// Let the model decide whether to call a tool.
    Auto,
    /// Require a tool call.
    Any,
    /// Prevent tool calls.
    None,
    /// Require one named tool.
    Tool {
        /// Tool name exposed to the provider.
        name: String,
    },
}

/// Fully lowered Anthropic Messages options (Architecture v2 part 2 §3.5).
#[derive(Clone, Debug, PartialEq)]
pub struct AnthropicOptions {
    /// Provider-visible response ceiling.
    pub max_tokens: u32,
    /// Temperature, omitted while thinking or when unsupported.
    pub temperature: Option<f32>,
    /// Provider/model-specific thinking plan.
    pub thinking: AnthropicThinking,
    /// Provider thinking display policy.
    pub thinking_display: AnthropicThinkingDisplay,
    /// Optional native tool choice. `None` preserves wire omission.
    pub tool_choice: Option<AnthropicToolChoice>,
    /// Prompt-cache retention selection.
    pub cache_retention: CacheRetention,
    /// Optional provider metadata user identifier.
    pub metadata_user_id: Option<String>,
    /// Whether interleaved-thinking beta behavior is requested by transport.
    pub interleaved_thinking: bool,
}

/// Typed simple-call patch for Anthropic Messages.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AnthropicSimplePatch {
    /// Thinking display override.
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    /// Provider metadata user identifier.
    pub metadata_user_id: Option<String>,
    /// Interleaved-thinking transport preference.
    pub interleaved_thinking: Option<bool>,
}

impl ApiFamily for AnthropicMessages {
    const API_ID: &'static str = "anthropic-messages";

    type Compat = AnthropicMessagesCompat;
    type ModelConfig = AnthropicMessagesModelConfig;
    type FullOptions = AnthropicOptions;
    type OptionsPatch = AnthropicSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(resolve_anthropic_messages_compat(model_overrides))
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        lower_anthropic_simple(context, simple, patch)
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_anthropic_messages(context, options)
    }
}

/// Applies pinned Pi's Anthropic defaults before typed model overrides.
pub fn resolve_anthropic_messages_compat(
    overrides: &AnthropicMessagesCompat,
) -> AnthropicMessagesCompat {
    AnthropicMessagesCompat {
        supports_eager_tool_input_streaming: Some(
            overrides
                .supports_eager_tool_input_streaming
                .unwrap_or(true),
        ),
        supports_long_cache_retention: Some(
            overrides.supports_long_cache_retention.unwrap_or(true),
        ),
        send_session_affinity_headers: Some(
            overrides.send_session_affinity_headers.unwrap_or(false),
        ),
        supports_cache_control_on_tools: Some(
            overrides.supports_cache_control_on_tools.unwrap_or(true),
        ),
        supports_temperature: Some(overrides.supports_temperature.unwrap_or(true)),
        force_adaptive_thinking: Some(overrides.force_adaptive_thinking.unwrap_or(false)),
        allow_empty_signature: Some(overrides.allow_empty_signature.unwrap_or(false)),
        supports_strict_tools: Some(overrides.supports_strict_tools.unwrap_or(false)),
        supports_tool_references: overrides.supports_tool_references,
        allowed_fallback_models: overrides.allowed_fallback_models.clone(),
        extensions: overrides.extensions.clone(),
    }
}

/// Inserts request-dependent Anthropic defaults before model/request header
/// overlays and the final Models-level transform.
///
/// Pinned Pi constructs beta and session-affinity defaults before merging
/// `model.headers` and `options.headers`. Keeping them in the control-plane
/// header phase preserves explicit deletion and final-transform precedence.
pub(crate) fn apply_anthropic_messages_default_headers(
    config: &AnthropicMessagesModelConfig,
    context: &Context,
    options: &SimpleGenerationOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    let interleaved_thinking = options
        .api_options
        .as_ref()
        .filter(|patch| {
            patch.api.as_str() == AnthropicMessages::API_ID && patch.schema_version == 1
        })
        .and_then(|patch| serde_json::from_str::<AnthropicSimplePatch>(patch.value.get()).ok())
        .and_then(|patch| patch.interleaved_thinking)
        .unwrap_or(true);

    apply_anthropic_messages_transport_headers(
        config,
        context,
        interleaved_thinking,
        options.cache_retention.unwrap_or_default(),
        options.session_id.as_deref(),
        headers,
    )
}

/// Applies request-dependent Anthropic headers for a fully typed call.
///
/// Full API-family callers use this transport-shaping hook before model and
/// request header overlays. In particular, an explicit
/// [`AnthropicOptions::interleaved_thinking`] value of `false` suppresses the
/// default interleaved-thinking beta exactly as pinned Pi does.
pub fn apply_anthropic_messages_full_options_headers(
    config: &AnthropicMessagesModelConfig,
    context: &Context,
    options: &AnthropicOptions,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    apply_anthropic_messages_full_options_request_headers(config, context, options, None, headers)
}

/// Applies fully typed Anthropic headers with common request transport data.
///
/// The separate session identifier mirrors Pi's `StreamOptions` inheritance
/// without mixing transport/auth controls into [`AnthropicOptions`].
pub fn apply_anthropic_messages_full_options_request_headers(
    config: &AnthropicMessagesModelConfig,
    context: &Context,
    options: &AnthropicOptions,
    session_id: Option<&str>,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    apply_anthropic_messages_transport_headers(
        config,
        context,
        options.interleaved_thinking,
        options.cache_retention,
        session_id,
        headers,
    )
}

fn apply_anthropic_messages_transport_headers(
    config: &AnthropicMessagesModelConfig,
    context: &Context,
    interleaved_thinking: bool,
    cache_retention: CacheRetention,
    session_id: Option<&str>,
    headers: &mut HeaderMap,
) -> Result<(), crate::MiddlewareError> {
    let compatibility = resolve_anthropic_messages_compat(&config.compat);

    let mut defaults = Vec::new();
    if !context.tools.is_empty() && compatibility.supports_eager_tool_input_streaming == Some(false)
    {
        defaults.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if interleaved_thinking && compatibility.force_adaptive_thinking != Some(true) {
        defaults.push(INTERLEAVED_THINKING_BETA);
    }
    if !compatibility.allowed_fallback_models.is_empty() {
        defaults.push(SERVER_SIDE_FALLBACK_BETA);
    }
    if !defaults.is_empty() {
        let mut values = headers
            .get("anthropic-beta")
            .map(|value| {
                value.to_str().map_err(|error| {
                    crate::MiddlewareError::new(
                        "invalid_header_value",
                        format!("anthropic-beta is not a string header: {error}"),
                    )
                })
            })
            .transpose()?
            .map(|value| value.split(',').map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        for default in defaults {
            if !values.iter().any(|value| value == default) {
                values.push(default.to_owned());
            }
        }
        let value = HeaderValue::from_str(&values.join(",")).map_err(|error| {
            crate::MiddlewareError::new(
                "invalid_header_value",
                format!("anthropic-beta cannot be encoded as a header: {error}"),
            )
        })?;
        headers.insert("anthropic-beta", value);
    }

    if compatibility.send_session_affinity_headers == Some(true)
        && cache_retention != CacheRetention::None
        && let Some(session_id) = session_id
    {
        let value = HeaderValue::from_str(session_id).map_err(|error| {
            crate::MiddlewareError::new(
                "invalid_header_value",
                format!("session ID cannot be encoded as a header: {error}"),
            )
        })?;
        headers.insert("x-session-affinity", value);
    }

    Ok(())
}

/// Lowers provider-neutral simple options according to pinned Pi's adaptive
/// versus budget-based Anthropic split.
pub fn lower_anthropic_simple(
    context: SimpleLoweringContext<'_, AnthropicMessages>,
    simple: &SimpleGenerationOptions,
    patch: &AnthropicSimplePatch,
) -> Result<AnthropicOptions, LoweringError> {
    let compatibility = context.compat;
    let (thinking, max_tokens) = match simple.reasoning {
        None | Some(ReasoningLevel::Off) => (
            AnthropicThinking::Disabled,
            clamp_requested_output(context, simple),
        ),
        Some(requested) if compatibility.force_adaptive_thinking == Some(true) => {
            let effort = resolve_effort(context.model, requested, simple.reasoning_fallback)?;
            (
                match effort {
                    None => AnthropicThinking::Disabled,
                    Some(effort) => AnthropicThinking::Adaptive {
                        effort: Some(effort),
                    },
                },
                clamp_requested_output(context, simple),
            )
        }
        Some(requested) => {
            let resolution = context
                .model
                .config
                .thinking_levels
                .resolve(requested, simple.reasoning_fallback)?;
            if matches!(
                resolution.support,
                Some(LevelSupport::Disabled | LevelSupport::Value(AnthropicThinkingValue::Off))
            ) {
                (
                    AnthropicThinking::Disabled,
                    clamp_requested_output(context, simple),
                )
            } else if let Some(LevelSupport::Value(AnthropicThinkingValue::Budget(budget))) =
                resolution.support
            {
                let max_tokens = expanded_max_tokens(context, simple, budget);
                (
                    AnthropicThinking::Budget {
                        budget_tokens: budget.min(max_tokens.saturating_sub(1_024)),
                    },
                    max_tokens,
                )
            } else {
                let plan = plan_thinking_budget(
                    simple.max_output_tokens,
                    context.model.common.limits.max_output_tokens,
                    resolution.effective,
                    &simple.thinking_budgets,
                )?;
                let max_tokens = clamp_output_to_context(context, plan.max_output_tokens);
                (
                    AnthropicThinking::Budget {
                        budget_tokens: plan.thinking_budget.min(max_tokens.saturating_sub(1_024)),
                    },
                    max_tokens,
                )
            }
        }
    };
    let thinking_enabled = matches!(
        thinking,
        AnthropicThinking::Adaptive { .. } | AnthropicThinking::Budget { .. }
    );
    let temperature = (!thinking_enabled && compatibility.supports_temperature != Some(false))
        .then_some(simple.temperature)
        .flatten();
    Ok(AnthropicOptions {
        max_tokens,
        temperature,
        thinking,
        thinking_display: patch.thinking_display.unwrap_or_default(),
        tool_choice: simple.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => AnthropicToolChoice::Auto,
            ToolChoice::None => AnthropicToolChoice::None,
        }),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        metadata_user_id: patch.metadata_user_id.clone(),
        interleaved_thinking: patch.interleaved_thinking.unwrap_or(true),
    })
}

fn clamp_requested_output(
    context: SimpleLoweringContext<'_, AnthropicMessages>,
    simple: &SimpleGenerationOptions,
) -> u32 {
    let requested = simple
        .max_output_tokens
        .unwrap_or(context.model.common.limits.max_output_tokens);
    clamp_output_to_context(context, requested)
}

fn expanded_max_tokens(
    context: SimpleLoweringContext<'_, AnthropicMessages>,
    simple: &SimpleGenerationOptions,
    budget: u32,
) -> u32 {
    let expanded = simple
        .max_output_tokens
        .map(|answer| answer.saturating_add(budget))
        .unwrap_or(context.model.common.limits.max_output_tokens)
        .min(context.model.common.limits.max_output_tokens);
    clamp_output_to_context(context, expanded)
}

fn clamp_output_to_context(
    context: SimpleLoweringContext<'_, AnthropicMessages>,
    requested: u32,
) -> u32 {
    if context.model.common.limits.context_window == 0 {
        requested.max(1)
    } else {
        requested.min(u32::try_from(context.available_context_tokens.max(1)).unwrap_or(u32::MAX))
    }
}

fn resolve_effort(
    model: &TypedModelDescriptor<AnthropicMessages>,
    requested: ReasoningLevel,
    fallback: crate::ReasoningFallback,
) -> Result<Option<AnthropicEffort>, LoweringError> {
    let resolved = model.config.thinking_levels.resolve(requested, fallback)?;
    Ok(match resolved.support {
        Some(LevelSupport::Disabled | LevelSupport::Value(AnthropicThinkingValue::Off)) => None,
        Some(LevelSupport::Value(AnthropicThinkingValue::Effort(effort))) => Some(effort),
        Some(LevelSupport::Value(AnthropicThinkingValue::Budget(_))) | None => {
            Some(default_effort(resolved.effective))
        }
        Some(LevelSupport::Unsupported) => unreachable!("resolve removes unsupported levels"),
    })
}

fn default_effort(level: ReasoningLevel) -> AnthropicEffort {
    match level {
        ReasoningLevel::Off | ReasoningLevel::Minimal | ReasoningLevel::Low => AnthropicEffort::Low,
        ReasoningLevel::Medium => AnthropicEffort::Medium,
        ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => AnthropicEffort::High,
    }
}

/// Encodes a projected context to the insertion order used by pinned Pi.
pub fn encode_anthropic_messages(
    context: EncodeContext<'_, AnthropicMessages>,
    options: &AnthropicOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    encode_anthropic_messages_with_system_prefix(context, options, None)
}

/// Encodes Anthropic Messages while prepending a provider-owned system block.
///
/// This narrow provider integration hook exists for Anthropic Claude Code
/// OAuth, whose pinned wire contract requires an identity block ahead of the
/// caller's system prompt. Ordinary API-family callers use
/// [`encode_anthropic_messages`] and pass no prefix.
pub fn encode_anthropic_messages_with_system_prefix(
    context: EncodeContext<'_, AnthropicMessages>,
    options: &AnthropicOptions,
    system_prefix: Option<&str>,
) -> Result<OrderedJsonObject, EncodeError> {
    let cache_control = cache_control(options.cache_retention, context.compat);
    let supports_tool_references = context
        .compat
        .supports_tool_references
        .unwrap_or_else(|| default_supports_tool_references(context.model));
    let (mut immediate_tools, mut deferred_tools) =
        split_deferred_tools(context.context, supports_tool_references);
    if immediate_tools.is_empty() && !deferred_tools.is_empty() {
        immediate_tools = std::mem::take(&mut deferred_tools);
    }
    let deferred_tool_names = deferred_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let mut request = OrderedJsonObject::new();
    request.insert("model", context.model.common.model_ref.model.as_str());
    request.insert(
        "messages",
        OrderedJsonValue::Array(convert_messages(
            context.context,
            cache_control.as_ref(),
            context.compat.allow_empty_signature == Some(true),
            &deferred_tool_names,
            &context.model.common.model_ref,
        )?),
    );
    request.insert("max_tokens", options.max_tokens);
    request.insert("stream", true);

    let prompt = context
        .context
        .system_prompt
        .as_deref()
        .filter(|v| !v.is_empty());
    if system_prefix.is_some() || prompt.is_some() {
        let mut blocks = Vec::new();
        if let Some(prefix) = system_prefix {
            blocks.push(system_block(prefix, cache_control.as_ref()));
        }
        if let Some(prompt) = prompt {
            blocks.push(system_block(prompt, cache_control.as_ref()));
        }
        request.insert("system", array(blocks));
    }
    if let Some(temperature) = options.temperature
        && matches!(
            options.thinking,
            AnthropicThinking::Omitted | AnthropicThinking::Disabled
        )
        && context.compat.supports_temperature.unwrap_or(true)
    {
        request.insert("temperature", temperature);
    }
    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let mut tools = convert_tools(
            &immediate_tools,
            context.compat,
            context
                .compat
                .supports_cache_control_on_tools
                .unwrap_or(true)
                .then_some(cache_control.as_ref())
                .flatten(),
            false,
        )?
        .into_iter()
        .collect::<Vec<_>>();
        tools.extend(convert_tools(&deferred_tools, context.compat, None, true)?);
        request.insert(
            "tools",
            OrderedJsonValue::Array(tools.into_iter().collect()),
        );
    }
    if context.model.common.reasoning {
        match &options.thinking {
            AnthropicThinking::Omitted => {}
            AnthropicThinking::Disabled => {
                if !matches!(
                    context.model.config.thinking_levels.off,
                    Some(LevelSupport::Unsupported)
                ) {
                    request.insert("thinking", object([("type", "disabled".into())]));
                }
            }
            AnthropicThinking::Adaptive { effort } => {
                request.insert(
                    "thinking",
                    object([
                        ("type", "adaptive".into()),
                        ("display", thinking_display(options.thinking_display).into()),
                    ]),
                );
                if let Some(effort) = effort {
                    request.insert(
                        "output_config",
                        object([("effort", effort_name(*effort).into())]),
                    );
                }
            }
            AnthropicThinking::Budget { budget_tokens } => {
                request.insert(
                    "thinking",
                    object([
                        ("type", "enabled".into()),
                        (
                            "budget_tokens",
                            if *budget_tokens == 0 {
                                1_024_u32.into()
                            } else {
                                (*budget_tokens).into()
                            },
                        ),
                        ("display", thinking_display(options.thinking_display).into()),
                    ]),
                );
            }
        }
    }
    if let Some(user_id) = options.metadata_user_id.as_deref() {
        request.insert("metadata", object([("user_id", user_id.into())]));
    }
    if let Some(choice) = options.tool_choice.as_ref() {
        request.insert("tool_choice", encode_tool_choice(choice));
    }
    if !context.compat.allowed_fallback_models.is_empty() {
        request.insert(
            "fallbacks",
            OrderedJsonValue::Array(
                context
                    .compat
                    .allowed_fallback_models
                    .iter()
                    .map(|fallback| object([("model", fallback.model.as_str().into())]))
                    .collect(),
            ),
        );
    }
    Ok(request)
}

fn cache_control(
    retention: CacheRetention,
    compatibility: &AnthropicMessagesCompat,
) -> Option<OrderedJsonValue> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Long if compatibility.supports_long_cache_retention.unwrap_or(true) => {
            Some(object([("type", "ephemeral".into()), ("ttl", "1h".into())]))
        }
        CacheRetention::Short | CacheRetention::Long => {
            Some(object([("type", "ephemeral".into())]))
        }
    }
}

fn convert_messages(
    context: &Context,
    cache_control: Option<&OrderedJsonValue>,
    allow_empty_signature: bool,
    deferred_tool_names: &BTreeSet<String>,
    target: &crate::ModelRef,
) -> Result<OrderedJsonArray, EncodeError> {
    let target_scope = ReplayScope::new(
        target.provider.clone(),
        AnthropicMessages::API_ID,
        target.model.clone(),
        target.model.clone(),
    );
    let mut messages = Vec::<OrderedJsonValue>::new();
    let mut loaded_tool_names = BTreeSet::new();
    let mut index = 0;
    while index < context.messages.len() {
        match &context.messages[index] {
            crate::Message::User(message) => {
                let blocks = user_blocks(&message.content);
                if blocks.is_empty() {
                    index += 1;
                    continue;
                }
                let content = match message.content.as_slice() {
                    [ContentBlock::Text { text, .. }] if !text.trim().is_empty() => {
                        OrderedJsonValue::from(text.as_str())
                    }
                    _ => array(blocks),
                };
                messages.push(object([("role", "user".into()), ("content", content)]));
            }
            crate::Message::Assistant(message) => {
                let blocks = assistant_blocks(message, &target_scope, allow_empty_signature)?;
                if !blocks.is_empty() {
                    messages.push(object([
                        ("role", "assistant".into()),
                        ("content", array(blocks)),
                    ]));
                }
            }
            crate::Message::ToolResult(_) => {
                let mut tool_results = Vec::new();
                let mut sibling_content = Vec::new();
                while let Some(crate::Message::ToolResult(message)) = context.messages.get(index) {
                    let converted =
                        tool_result_block(message, deferred_tool_names, &mut loaded_tool_names);
                    tool_results.push(converted.tool_result);
                    sibling_content.extend(converted.sibling_content);
                    index += 1;
                }
                index -= 1;
                tool_results.extend(sibling_content);
                messages.push(object([
                    ("role", "user".into()),
                    ("content", array(tool_results)),
                ]));
            }
        }
        index += 1;
    }
    if let (Some(cache), Some(OrderedJsonValue::Object(last))) =
        (cache_control, messages.last_mut())
        && last
            .get("role")
            .and_then(OrderedJsonValue::as_string)
            .and_then(|v| v.to_utf8().ok())
            .as_deref()
            == Some("user")
        && let Some(content) = last.get_mut("content")
    {
        match content {
            OrderedJsonValue::String(text) => {
                *content = array(vec![object([
                    ("type", "text".into()),
                    ("text", OrderedJsonValue::String(text.clone())),
                    ("cache_control", cache.clone()),
                ])]);
            }
            OrderedJsonValue::Array(blocks) => {
                if let Some(OrderedJsonValue::Object(block)) = blocks.as_mut_slice().last_mut()
                    && matches!(
                        json_string(block.get("type")).as_deref(),
                        Some("text" | "image" | "tool_result")
                    )
                {
                    block.insert("cache_control", cache.clone());
                }
            }
            _ => {}
        }
    }
    Ok(messages.into_iter().collect())
}

fn user_blocks(content: &[ContentBlock]) -> Vec<OrderedJsonValue> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                blocks.push(object([
                    ("type", "text".into()),
                    ("text", text.as_str().into()),
                ]));
            }
            ContentBlock::Image {
                data, mime_type, ..
            } => {
                blocks.push(object([
                    ("type", "image".into()),
                    (
                        "source",
                        object([
                            ("type", "base64".into()),
                            ("media_type", mime_type.as_str().into()),
                            ("data", data.as_str().into()),
                        ]),
                    ),
                ]));
            }
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ToolCall { .. } => {}
        }
    }
    blocks
}

fn assistant_blocks(
    message: &crate::AssistantMessage,
    target: &ReplayScope,
    allow_empty_signature: bool,
) -> Result<Vec<OrderedJsonValue>, EncodeError> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                blocks.push(object([
                    ("type", "text".into()),
                    ("text", text.as_str().into()),
                ]));
            }
            ContentBlock::Thinking {
                id, text, redacted, ..
            } => {
                let kind = if *redacted {
                    ANTHROPIC_REDACTED_THINKING_KIND
                } else {
                    ANTHROPIC_THINKING_SIGNATURE_KIND
                };
                let replay = message.replay.complete_item_for_block(id, kind, target);
                if *redacted {
                    let data = replay
                        .and_then(crate::ReplayItem::as_utf8)
                        .ok_or(EncodeError::MissingRedactedThinkingPayload)?;
                    blocks.push(object([
                        ("type", "redacted_thinking".into()),
                        ("data", data.into()),
                    ]));
                } else {
                    let signature = replay.and_then(crate::ReplayItem::as_utf8);
                    let has_signature = signature.is_some_and(|value| !value.trim().is_empty());
                    if text.trim().is_empty() && !has_signature {
                        continue;
                    }
                    if has_signature || allow_empty_signature {
                        let signature = if has_signature {
                            signature.expect("non-empty signature was checked")
                        } else {
                            ""
                        };
                        blocks.push(object([
                            ("type", "thinking".into()),
                            ("thinking", text.as_str().into()),
                            ("signature", signature.into()),
                        ]));
                    } else {
                        blocks.push(object([
                            ("type", "text".into()),
                            ("text", text.as_str().into()),
                        ]));
                    }
                }
            }
            ContentBlock::ToolCall { call, .. } => {
                blocks.push(object([
                    ("type", "tool_use".into()),
                    ("id", call.id.as_str().into()),
                    ("name", call.name.as_str().into()),
                    (
                        "input",
                        if call.arguments.is_null() {
                            object([])
                        } else {
                            call.arguments.clone().into()
                        },
                    ),
                ]));
            }
            ContentBlock::Text { .. } | ContentBlock::Image { .. } => {}
        }
    }
    Ok(blocks)
}

struct ConvertedToolResult {
    tool_result: OrderedJsonValue,
    sibling_content: Vec<OrderedJsonValue>,
}

fn tool_result_block(
    message: &crate::ToolResultMessage,
    deferred_tool_names: &BTreeSet<String>,
    loaded_tool_names: &mut BTreeSet<String>,
) -> ConvertedToolResult {
    let references = message
        .added_tool_names
        .iter()
        .filter(|name| {
            deferred_tool_names.contains(name.as_str()) && loaded_tool_names.insert((*name).clone())
        })
        .map(|name| {
            object([
                ("type", "tool_reference".into()),
                ("tool_name", name.as_str().into()),
            ])
        })
        .collect::<Vec<_>>();
    let converted_content = tool_result_content(&message.content);
    let sibling_content = if references.is_empty() {
        Vec::new()
    } else {
        content_as_blocks(converted_content.clone())
    };
    ConvertedToolResult {
        tool_result: object([
            ("type", "tool_result".into()),
            ("tool_use_id", message.tool_call_id.as_str().into()),
            (
                "content",
                if references.is_empty() {
                    converted_content
                } else {
                    array(references)
                },
            ),
            ("is_error", message.is_error.into()),
        ]),
        sibling_content,
    }
}

fn tool_result_content(content: &[ToolResultContent]) -> OrderedJsonValue {
    if content
        .iter()
        .all(|block| matches!(block, ToolResultContent::Text { .. }))
    {
        return content
            .iter()
            .filter_map(|block| match block {
                ToolResultContent::Text { text, .. } => Some(text.as_str()),
                ToolResultContent::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into();
    }
    let mut blocks = Vec::new();
    if content
        .iter()
        .all(|block| matches!(block, ToolResultContent::Image { .. }))
        && !content.is_empty()
    {
        blocks.push(object([
            ("type", "text".into()),
            ("text", "(see attached image)".into()),
        ]));
    }
    for block in content {
        match block {
            ToolResultContent::Text { text, .. } => blocks.push(object([
                ("type", "text".into()),
                ("text", text.as_str().into()),
            ])),
            ToolResultContent::Image {
                data, mime_type, ..
            } => blocks.push(object([
                ("type", "image".into()),
                (
                    "source",
                    object([
                        ("type", "base64".into()),
                        ("media_type", mime_type.as_str().into()),
                        ("data", data.as_str().into()),
                    ]),
                ),
            ])),
        }
    }
    array(blocks)
}

fn content_as_blocks(content: OrderedJsonValue) -> Vec<OrderedJsonValue> {
    match content {
        OrderedJsonValue::Array(blocks) => blocks.into_iter().collect(),
        OrderedJsonValue::String(text) => vec![object([
            ("type", "text".into()),
            ("text", OrderedJsonValue::String(text)),
        ])],
        _ => Vec::new(),
    }
}

fn convert_tools(
    tools: &[crate::ToolSpec],
    compatibility: &AnthropicMessagesCompat,
    cache_control: Option<&OrderedJsonValue>,
    defer_loading: bool,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut wire = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let (parameters, strict) = resolve_tool_schema(tool, compatibility)?;
        let mut legacy = OrderedJsonObject::new();
        legacy.insert("type", "object");
        legacy.insert(
            "properties",
            parameters
                .get("properties")
                .filter(|value| !matches!(value, OrderedJsonValue::Null))
                .cloned()
                .unwrap_or_else(|| object([])),
        );
        legacy.insert(
            "required",
            parameters
                .get("required")
                .filter(|value| !matches!(value, OrderedJsonValue::Null))
                .cloned()
                .unwrap_or_else(|| array(Vec::new())),
        );
        let input_schema = if strict {
            let mut merged = parameters;
            merged.insert("type", "object");
            merged.insert(
                "properties",
                legacy.get("properties").cloned().expect("inserted"),
            );
            merged.insert(
                "required",
                legacy.get("required").cloned().expect("inserted"),
            );
            merged
        } else {
            legacy
        };
        let mut definition = OrderedJsonObject::new();
        definition.insert("name", tool.name.as_str());
        definition.insert("description", tool.description.as_str());
        if compatibility
            .supports_eager_tool_input_streaming
            .unwrap_or(true)
        {
            definition.insert("eager_input_streaming", true);
        }
        if strict {
            definition.insert("strict", true);
        }
        definition.insert("input_schema", input_schema);
        if defer_loading {
            definition.insert("defer_loading", true);
        }
        if index + 1 == tools.len()
            && let Some(cache) = cache_control
        {
            definition.insert("cache_control", cache.clone());
        }
        wire.push(OrderedJsonValue::Object(definition));
    }
    Ok(wire.into_iter().collect())
}

fn split_deferred_tools(
    context: &Context,
    enabled: bool,
) -> (Vec<crate::ToolSpec>, Vec<crate::ToolSpec>) {
    let mut unique_tools = Vec::<crate::ToolSpec>::new();
    for tool in &context.tools {
        if let Some(existing) = unique_tools.iter_mut().find(|item| item.name == tool.name) {
            *existing = tool.clone();
        } else {
            unique_tools.push(tool.clone());
        }
    }
    if !enabled {
        return (unique_tools, Vec::new());
    }

    let mut used_names = BTreeSet::new();
    let mut deferred_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            crate::Message::Assistant(message) => {
                for block in &message.content {
                    if let ContentBlock::ToolCall { call, .. } = block {
                        used_names.insert(call.name.clone());
                    }
                }
            }
            crate::Message::ToolResult(message) => {
                for name in &message.added_tool_names {
                    if !used_names.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            crate::Message::User(_) => {}
        }
    }

    unique_tools
        .into_iter()
        .partition(|tool| !deferred_names.contains(&tool.name))
}

fn default_supports_tool_references(model: &TypedModelDescriptor<AnthropicMessages>) -> bool {
    if model.common.model_ref.provider.as_str() != "anthropic"
        || model.common.model_ref.model.as_str().contains("haiku")
    {
        return false;
    }
    let model_id = model.common.model_ref.model.as_str();
    let Some(version) = model_id.strip_prefix("claude-").and_then(|rest| {
        ["opus-", "sonnet-", "fable-"]
            .iter()
            .find_map(|prefix| rest.strip_prefix(prefix))
    }) else {
        return false;
    };
    let mut parts = version.split('-');
    let Some(major) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let minor = parts
        .next()
        .filter(|value| value.len() < 8)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    major > 4 || (major == 4 && minor >= 5)
}

fn resolve_tool_schema(
    tool: &crate::ToolSpec,
    compatibility: &AnthropicMessagesCompat,
) -> Result<(OrderedJsonObject, bool), EncodeError> {
    let requested = match tool.constrained_sampling {
        Some(crate::ConstrainedSampling::Config(
            crate::ConstrainedSamplingConfig::JsonSchema { strict },
        )) => Some(strict),
        _ => None,
    };
    let Some(preference) = requested else {
        return Ok((json_object(tool.parameters.clone())?, false));
    };
    if compatibility.supports_strict_tools != Some(true) {
        return match preference {
            crate::JsonSchemaStrictMode::Prefer => {
                Ok((json_object(tool.parameters.clone())?, false))
            }
            crate::JsonSchemaStrictMode::Require => Err(EncodeError::InvalidRequest {
                message: format!(
                    "tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported",
                    tool.name
                ),
            }),
        };
    }
    match make_strict_json_schema(&tool.parameters) {
        Ok(schema) => Ok((json_object(schema)?, true)),
        Err(_) if preference == crate::JsonSchemaStrictMode::Prefer => {
            Ok((json_object(tool.parameters.clone())?, false))
        }
        Err(message) => Err(EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" requires JSON-schema constrained sampling, but {message}",
                tool.name
            ),
        }),
    }
}

fn system_block(text: &str, cache: Option<&OrderedJsonValue>) -> OrderedJsonValue {
    let mut block = OrderedJsonObject::new();
    block.insert("type", "text");
    block.insert("text", text);
    if let Some(cache) = cache {
        block.insert("cache_control", cache.clone());
    }
    block.into()
}

fn object<const N: usize>(entries: [(&str, OrderedJsonValue); N]) -> OrderedJsonValue {
    OrderedJsonValue::Object(entries.into_iter().collect())
}

fn array(values: Vec<OrderedJsonValue>) -> OrderedJsonValue {
    OrderedJsonValue::Array(values.into_iter().collect())
}

fn json_object(value: serde_json::Value) -> Result<OrderedJsonObject, EncodeError> {
    match OrderedJsonValue::from(value) {
        OrderedJsonValue::Object(value) => Ok(value),
        _ => Err(EncodeError::InvalidRequest {
            message: "tool parameters must be a JSON object".to_owned(),
        }),
    }
}

fn json_string(value: Option<&OrderedJsonValue>) -> Option<String> {
    value
        .and_then(OrderedJsonValue::as_string)
        .and_then(|value| value.to_utf8().ok())
}

fn effort_name(effort: AnthropicEffort) -> &'static str {
    match effort {
        AnthropicEffort::Minimal => "minimal",
        AnthropicEffort::Low => "low",
        AnthropicEffort::Medium => "medium",
        AnthropicEffort::High => "high",
        AnthropicEffort::Xhigh => "xhigh",
        AnthropicEffort::Max => "max",
    }
}

fn thinking_display(display: AnthropicThinkingDisplay) -> &'static str {
    match display {
        AnthropicThinkingDisplay::Summarized => "summarized",
        AnthropicThinkingDisplay::Omitted => "omitted",
    }
}

fn encode_tool_choice(choice: &AnthropicToolChoice) -> OrderedJsonValue {
    match choice {
        AnthropicToolChoice::Auto => object([("type", "auto".into())]),
        AnthropicToolChoice::Any => object([("type", "any".into())]),
        AnthropicToolChoice::None => object([("type", "none".into())]),
        AnthropicToolChoice::Tool { name } => {
            object([("type", "tool".into()), ("name", name.as_str().into())])
        }
    }
}

/// Anthropic Messages tool-call identifier policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicToolCallIdPolicy;

impl ToolCallIdPolicy for AnthropicToolCallIdPolicy {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        let normalized: String = original
            .as_str()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        Ok(ToolCallId::new(normalized))
    }
}

/// Anthropic Messages replay and handoff hooks.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicMessagesHandoff;

impl ApiFamilyHandoff for AnthropicMessagesHandoff {
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool {
        matches!(
            kind.as_str(),
            ANTHROPIC_THINKING_SIGNATURE_KIND | ANTHROPIC_REDACTED_THINKING_KIND
        )
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        &AnthropicToolCallIdPolicy
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}
