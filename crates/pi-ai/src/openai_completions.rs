//! OpenAI-compatible Chat Completions family lowering and wire encoding.
//!
//! This module realizes Architecture v2 part 2 §1.5, §3.6, §5.1, and
//! §10.2/§10.5/§10.8. Transport and SSE decoding remain in the leaf
//! `pi-ai-openai` crate; the typed family contract and byte-stable request
//! representation live here so every compatible provider shares them.

use crate::{
    ApiFamily, ApiFamilyHandoff, AssistantMessage, CacheControlFormat, CacheRetention,
    ChatTemplateKwargValue, ChatTemplateValues, ContentBlock, Context, EncodeContext, EncodeError,
    HandoffError, HandoffReport, LevelSupport, LoweringError, MIN_ANSWER_TOKENS, MaxTokensField,
    MiddlewareError, ModelFingerprint, OpenAiCompletionsCompat, OpenAiCompletionsModelConfig,
    OpenAiThinkingFormat, OpenAiThinkingValue, OpenRouterRouting, OrderedJsonArray,
    OrderedJsonObject, OrderedJsonValue, ReasoningLevel, ReplayApplicability, ReplayCompleteness,
    ReplayItem, ReplayItemId, ReplayKind, ReplayScope, ReplayTarget, SessionAffinityFormat,
    SimpleGenerationOptions, SimpleLoweringContext, ThinkingTokenBudgetField, ToolCallId,
    ToolCallIdPolicy, ToolChoice, ToolResultContent, TypedModelDescriptor, VercelGatewayRouting,
    parse_ordered_json,
};
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use url::Url;

/// Replay kind retaining the concrete visible-reasoning field selected by an
/// OpenAI-compatible response.
pub const OPENAI_CHAT_REASONING_FIELD_KIND: &str = "openai.chat.reasoning-field";

/// Replay kind retaining one ordered `reasoning_details` member.
pub const OPENAI_CHAT_REASONING_DETAIL_KIND: &str = "openai.chat.reasoning-detail";

/// Maximum number of Unicode scalar values in OpenAI's prompt-cache key.
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// Marker type for the `openai-completions` API family.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiCompletions;

/// Fully lowered OpenAI-compatible Chat Completions options
/// (Architecture v2 part 2 §3.6).
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiCompletionsOptions {
    /// Optional provider-visible output cap.
    pub max_tokens: Option<u32>,
    /// Provider field receiving [`Self::max_tokens`].
    pub max_tokens_field: MaxTokensField,
    /// Provider/model-specific reasoning plan.
    pub reasoning: OpenAiReasoningPlan,
    /// Named temperature inserted before tools and reasoning fields.
    pub temperature: Option<f32>,
    /// Ordered `samplingParams` overlay applied after every named field.
    pub sampling: OrderedJsonObject,
    /// Optional native OpenAI Chat Completions tool selection.
    ///
    /// This is intentionally wider than the provider-neutral simple
    /// [`ToolChoice`] contract. `None` omits `tool_choice` from the request,
    /// preserving the distinction between an unspecified choice and an
    /// explicitly supplied [`OpenAiCompletionsToolChoice::Auto`].
    pub tool_choice: Option<OpenAiCompletionsToolChoice>,
    /// Prompt-cache retention selection.
    pub cache_retention: CacheRetention,
    /// Optional provider session-affinity key.
    pub session_id: Option<String>,
}

/// Native `openai-completions` tool-choice options accepted by pinned Pi's
/// full API-specific stream surface.
#[derive(Clone, Debug, PartialEq)]
pub enum OpenAiCompletionsToolChoice {
    /// Let the model decide whether to call a tool.
    Auto,
    /// Prevent tool calls.
    None,
    /// Require the model to call one or more tools.
    Required,
    /// Force one named function tool.
    Function {
        /// Function name to force.
        name: String,
    },
    /// Force one named custom/grammar tool.
    Custom {
        /// Custom tool name to force.
        name: String,
    },
    /// Restrict the model to an explicit subset of native tool definitions.
    AllowedTools {
        /// Whether the model may choose a message or must call an allowed
        /// tool.
        mode: OpenAiAllowedToolsMode,
        /// Insertion-ordered native OpenAI tool references.
        tools: OrderedJsonArray,
    },
}

impl From<ToolChoice> for OpenAiCompletionsToolChoice {
    fn from(value: ToolChoice) -> Self {
        match value {
            ToolChoice::Auto => Self::Auto,
            ToolChoice::None => Self::None,
        }
    }
}

/// Selection mode inside an OpenAI `allowed_tools` tool choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiAllowedToolsMode {
    /// The model may either emit a message or call an allowed tool.
    Auto,
    /// The model must call one or more allowed tools.
    Required,
}

/// Lossless OpenAI-compatible reasoning request plan.
///
/// Pinned Pi applies the format-specific fields first and an optional
/// top-level token budget independently. Representing those passes as a
/// product prevents one from erasing the other.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiReasoningPlan {
    /// Format-specific enable/effort fields.
    pub mode: OpenAiReasoningMode,
    /// Independent top-level token budget, when configured.
    pub token_budget: Option<OpenAiReasoningTokenBudget>,
}

impl OpenAiReasoningPlan {
    /// Creates a plan with reasoning disabled and no token budget.
    pub fn disabled() -> Self {
        Self {
            mode: OpenAiReasoningMode::Disabled,
            token_budget: None,
        }
    }
}

/// Format-specific portion of an OpenAI-compatible reasoning request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenAiReasoningMode {
    /// No positive reasoning request. A typed model's explicit `off` mapping
    /// may still produce a provider-visible disable value.
    Disabled,
    /// OpenAI-style top-level reasoning effort.
    ReasoningEffort {
        /// Provider-native effort string.
        effort: String,
        /// Whether `effort` came from an explicit string in the model's
        /// thinking-level map or from the requested level itself.
        provenance: OpenAiReasoningEffortProvenance,
    },
    /// Reasoning is enabled by the requested level, but the model mapping
    /// supplies a numeric token budget rather than a provider-native effort
    /// string.
    ///
    /// This remains distinct from [`Self::Disabled`]: Zai, Qwen, Together,
    /// and other switch-based formats must still receive their positive
    /// enable field while the independent token-budget pass emits the cap.
    Enabled,
    /// OpenRouter nested reasoning object.
    OpenRouter {
        /// Provider-native effort string.
        effort: String,
    },
    /// DeepSeek thinking switch and optional effort.
    DeepSeek {
        /// Whether thinking is enabled.
        enabled: bool,
        /// Optional provider-native effort.
        effort: Option<String>,
    },
    /// Configurable chat-template keyword arguments.
    ChatTemplate {
        /// Resolved insertion-ordered keyword arguments.
        kwargs: OrderedJsonObject,
        /// Baseten's independently emitted mapped reasoning effort.
        reasoning_effort: Option<String>,
    },
    /// Top-level string-valued thinking control.
    StringThinking {
        /// Provider-native value.
        value: String,
    },
}

/// Provenance of an OpenAI-compatible reasoning effort.
///
/// Most compatible APIs fall back to the requested effort when a model map
/// has no entry. Ant Ling is the exception in pinned Pi: it emits its nested
/// `reasoning` object only for an explicitly mapped string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiReasoningEffortProvenance {
    /// The effort is the caller's requested level because no map entry was
    /// present.
    RequestedLevel,
    /// The effort is an explicit string from the model's thinking-level map.
    ModelMapping,
}

/// Independent top-level reasoning token budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenAiReasoningTokenBudget {
    /// Provider field receiving the budget.
    pub field: ThinkingTokenBudgetField,
    /// Reasoning token budget.
    pub budget: u32,
}

/// One typed patch applied after common OpenAI simple-option lowering.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCompletionsSimplePatch {
    /// Ordered API-specific sampling fields applied after model/request values.
    pub sampling: OrderedJsonObject,
}

impl ApiFamily for OpenAiCompletions {
    const API_ID: &'static str = "openai-completions";

    type Compat = OpenAiCompletionsCompat;
    type ModelConfig = OpenAiCompletionsModelConfig;
    type FullOptions = OpenAiCompletionsOptions;
    type OptionsPatch = OpenAiCompletionsSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(resolve_openai_completions_compat(
            effective_base_url,
            model_overrides,
        ))
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        lower_openai_completions_simple(context, simple, patch)
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_openai_completions(context, options)
    }
}

/// Resolves URL-detected compatibility defaults and then overlays the typed
/// model configuration. Detection deliberately uses the effective URL after
/// authentication, as required by Architecture v2 part 2 §3.6.
pub fn resolve_openai_completions_compat(
    effective_base_url: &Url,
    overrides: &OpenAiCompletionsCompat,
) -> OpenAiCompletionsCompat {
    let base_url = effective_base_url.as_str();
    let lowercase = base_url.to_ascii_lowercase();
    let is_zai = base_url.contains("api.z.ai") || base_url.contains("open.bigmodel.cn");
    let is_together = base_url.contains("api.together.ai") || base_url.contains("api.together.xyz");
    let is_moonshot = base_url.contains("api.moonshot.");
    let is_openrouter = base_url.contains("openrouter.ai");
    let is_cloudflare_workers = base_url.contains("api.cloudflare.com");
    let is_cloudflare_gateway = base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = base_url.contains("api.ant-ling.com");
    let is_deepseek = lowercase.contains("deepseek.com");
    let is_grok = base_url.contains("api.x.ai");
    let is_nonstandard = is_nvidia
        || base_url.contains("cerebras.ai")
        || is_grok
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers
        || is_cloudflare_gateway
        || is_ant_ling;
    let uses_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;
    let thinking_format = if is_deepseek {
        OpenAiThinkingFormat::DeepSeek
    } else if is_zai {
        OpenAiThinkingFormat::Zai
    } else if is_together {
        OpenAiThinkingFormat::Together
    } else if is_ant_ling {
        OpenAiThinkingFormat::AntLing
    } else if is_openrouter {
        OpenAiThinkingFormat::OpenRouter
    } else {
        OpenAiThinkingFormat::OpenAi
    };
    let detected = OpenAiCompletionsCompat {
        supports_store: Some(!is_nonstandard),
        supports_developer_role: Some(!is_nonstandard && !is_openrouter),
        supports_reasoning_effort: Some(
            !is_grok
                && !is_zai
                && !is_moonshot
                && !is_together
                && !is_cloudflare_gateway
                && !is_nvidia
                && !is_ant_ling,
        ),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(if uses_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        }),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        requires_reasoning_content_on_assistant_messages: Some(is_deepseek),
        thinking_format: Some(thinking_format),
        chat_template_kwargs: Some(ChatTemplateValues::new()),
        chat_template_args: Some(ChatTemplateValues::new()),
        open_router_routing: Some(OpenRouterRouting::default()),
        vercel_gateway_routing: Some(VercelGatewayRouting::default()),
        zai_tool_stream: Some(false),
        thinking_token_budget_field: None,
        supports_thinking_token_budget: Some(false),
        supports_strict_mode: Some(
            !is_moonshot && !is_together && !is_cloudflare_gateway && !is_nvidia,
        ),
        supports_openai_grammar_tools: Some(false),
        cache_control_format: None,
        send_session_affinity_headers: Some(false),
        deferred_tools_mode: None,
        session_affinity_format: Some(if is_openrouter {
            SessionAffinityFormat::OpenRouter
        } else {
            SessionAffinityFormat::OpenAi
        }),
        supports_long_cache_retention: Some(
            !(is_together
                || is_cloudflare_workers
                || is_cloudflare_gateway
                || is_nvidia
                || is_ant_ling),
        ),
        extensions: Default::default(),
    };
    overlay_compat(detected, overrides)
}

fn overlay_compat(
    mut detected: OpenAiCompletionsCompat,
    overrides: &OpenAiCompletionsCompat,
) -> OpenAiCompletionsCompat {
    macro_rules! overlay {
        ($($field:ident),+ $(,)?) => {
            $(if overrides.$field.is_some() {
                detected.$field = overrides.$field.clone();
            })+
        };
    }
    overlay!(
        supports_store,
        supports_developer_role,
        supports_reasoning_effort,
        supports_usage_in_streaming,
        supports_finish_reason,
        max_tokens_field,
        requires_tool_result_name,
        requires_assistant_after_tool_result,
        requires_thinking_as_text,
        requires_reasoning_content_on_assistant_messages,
        thinking_format,
        chat_template_kwargs,
        chat_template_args,
        open_router_routing,
        vercel_gateway_routing,
        zai_tool_stream,
        thinking_token_budget_field,
        supports_thinking_token_budget,
        supports_strict_mode,
        supports_openai_grammar_tools,
        cache_control_format,
        send_session_affinity_headers,
        deferred_tools_mode,
        session_affinity_format,
        supports_long_cache_retention,
    );
    detected.extensions.extend(overrides.extensions.clone());
    detected
}

fn lower_openai_completions_simple(
    context: SimpleLoweringContext<'_, OpenAiCompletions>,
    simple: &SimpleGenerationOptions,
    patch: &OpenAiCompletionsSimplePatch,
) -> Result<OpenAiCompletionsOptions, LoweringError> {
    let requested = simple
        .max_output_tokens
        .unwrap_or(context.model.common.limits.max_output_tokens);
    let max_tokens = if context.model.common.limits.context_window == 0 {
        requested.max(1)
    } else {
        requested.min(u32::try_from(context.available_context_tokens.max(1)).unwrap_or(u32::MAX))
    };

    let mut sampling = context.model.config.sampling_defaults.clone();
    for (name, value) in &simple.sampling {
        sampling.insert(name.clone(), value.clone());
    }
    for (name, value) in &patch.sampling {
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

    let reasoning = lower_reasoning(context.model, context.compat, simple, max_tokens)?;
    Ok(OpenAiCompletionsOptions {
        max_tokens: Some(max_tokens),
        max_tokens_field: context
            .compat
            .max_tokens_field
            .unwrap_or(MaxTokensField::MaxCompletionTokens),
        reasoning,
        temperature: simple.temperature,
        sampling,
        tool_choice: simple.tool_choice.map(Into::into),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
    })
}

fn lower_reasoning(
    model: &TypedModelDescriptor<OpenAiCompletions>,
    compat: &OpenAiCompletionsCompat,
    simple: &SimpleGenerationOptions,
    max_tokens: u32,
) -> Result<OpenAiReasoningPlan, LoweringError> {
    let Some(requested) = simple
        .reasoning
        .filter(|level| *level != ReasoningLevel::Off)
    else {
        return Ok(OpenAiReasoningPlan::disabled());
    };
    if !model.common.reasoning {
        return Ok(OpenAiReasoningPlan::disabled());
    }

    let resolution = model
        .config
        .thinking_levels
        .resolve(requested, simple.reasoning_fallback)?;
    let (mapped, effort_provenance) = match resolution.support {
        Some(LevelSupport::Unsupported) => {
            return Err(LoweringError::UnsupportedReasoningLevel { requested });
        }
        Some(LevelSupport::Disabled) | Some(LevelSupport::Value(OpenAiThinkingValue::Disabled)) => {
            return Ok(OpenAiReasoningPlan::disabled());
        }
        Some(LevelSupport::Value(value)) => (value, OpenAiReasoningEffortProvenance::ModelMapping),
        None => (
            OpenAiThinkingValue::Effort(reasoning_level_name(resolution.effective).to_owned()),
            OpenAiReasoningEffortProvenance::RequestedLevel,
        ),
    };

    let mapped_budget = match &mapped {
        OpenAiThinkingValue::TokenBudget(budget) => Some(*budget),
        OpenAiThinkingValue::Disabled | OpenAiThinkingValue::Effort(_) => None,
    };
    let thinking_budget = mapped_budget
        .or_else(|| simple.thinking_budgets.budget_for(resolution.effective))
        .map(|budget| budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS)))
        .filter(|budget| *budget > 0);

    let token_budget = match (resolve_thinking_budget_field(compat), thinking_budget) {
        (Some(field), Some(budget)) => Some(OpenAiReasoningTokenBudget { field, budget }),
        (None, Some(_)) if mapped_budget.is_some() => {
            return Err(LoweringError::InvalidConfiguration {
                message:
                    "model maps reasoning to a token budget but compatibility has no budget field"
                        .into(),
            });
        }
        _ => None,
    };

    let thinking_format = compat
        .thinking_format
        .unwrap_or(OpenAiThinkingFormat::OpenAi);
    let mode = match mapped {
        OpenAiThinkingValue::TokenBudget(_) => match thinking_format {
            OpenAiThinkingFormat::DeepSeek => OpenAiReasoningMode::DeepSeek {
                enabled: true,
                effort: None,
            },
            OpenAiThinkingFormat::QwenChatTemplate => OpenAiReasoningMode::ChatTemplate {
                kwargs: OrderedJsonObject::from_iter([
                    ("enable_thinking", OrderedJsonValue::from(true)),
                    ("preserve_thinking", OrderedJsonValue::from(true)),
                ]),
                reasoning_effort: None,
            },
            OpenAiThinkingFormat::ChatTemplate | OpenAiThinkingFormat::Baseten => {
                OpenAiReasoningMode::ChatTemplate {
                    kwargs: resolve_chat_template_values(
                        model,
                        compat,
                        true,
                        None,
                        thinking_budget,
                    )?,
                    reasoning_effort: None,
                }
            }
            OpenAiThinkingFormat::OpenAi
            | OpenAiThinkingFormat::OpenRouter
            | OpenAiThinkingFormat::Together
            | OpenAiThinkingFormat::Zai
            | OpenAiThinkingFormat::Qwen
            | OpenAiThinkingFormat::AntLing
            | OpenAiThinkingFormat::StringThinking => OpenAiReasoningMode::Enabled,
        },
        OpenAiThinkingValue::Disabled => OpenAiReasoningMode::Disabled,
        OpenAiThinkingValue::Effort(effort) => match thinking_format {
            OpenAiThinkingFormat::OpenRouter => OpenAiReasoningMode::OpenRouter { effort },
            OpenAiThinkingFormat::DeepSeek => OpenAiReasoningMode::DeepSeek {
                enabled: true,
                effort: compat
                    .supports_reasoning_effort
                    .unwrap_or(false)
                    .then_some(effort),
            },
            OpenAiThinkingFormat::StringThinking => {
                OpenAiReasoningMode::StringThinking { value: effort }
            }
            OpenAiThinkingFormat::QwenChatTemplate => OpenAiReasoningMode::ChatTemplate {
                kwargs: OrderedJsonObject::from_iter([
                    ("enable_thinking", OrderedJsonValue::from(true)),
                    ("preserve_thinking", OrderedJsonValue::from(true)),
                ]),
                reasoning_effort: None,
            },
            OpenAiThinkingFormat::ChatTemplate | OpenAiThinkingFormat::Baseten => {
                let reasoning_effort = (thinking_format == OpenAiThinkingFormat::Baseten
                    && compat.supports_reasoning_effort.unwrap_or(false))
                .then(|| effort.clone());
                OpenAiReasoningMode::ChatTemplate {
                    kwargs: resolve_chat_template_values(
                        model,
                        compat,
                        true,
                        Some(&effort),
                        thinking_budget,
                    )?,
                    reasoning_effort,
                }
            }
            OpenAiThinkingFormat::OpenAi
            | OpenAiThinkingFormat::Together
            | OpenAiThinkingFormat::Zai
            | OpenAiThinkingFormat::Qwen
            | OpenAiThinkingFormat::AntLing => OpenAiReasoningMode::ReasoningEffort {
                effort,
                provenance: effort_provenance,
            },
        },
    };
    Ok(OpenAiReasoningPlan { mode, token_budget })
}

fn resolve_thinking_budget_field(
    compat: &OpenAiCompletionsCompat,
) -> Option<ThinkingTokenBudgetField> {
    compat.thinking_token_budget_field.or_else(|| {
        compat
            .supports_thinking_token_budget
            .unwrap_or(false)
            .then_some(ThinkingTokenBudgetField::ThinkingTokenBudget)
    })
}

fn reasoning_level_name(level: ReasoningLevel) -> &'static str {
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

fn resolve_chat_template_values(
    model: &TypedModelDescriptor<OpenAiCompletions>,
    compat: &OpenAiCompletionsCompat,
    enabled: bool,
    effort: Option<&str>,
    thinking_budget: Option<u32>,
) -> Result<OrderedJsonObject, LoweringError> {
    let values = match compat
        .thinking_format
        .unwrap_or(OpenAiThinkingFormat::OpenAi)
    {
        OpenAiThinkingFormat::Baseten => compat.chat_template_args.as_ref(),
        _ => compat.chat_template_kwargs.as_ref(),
    };
    let mut resolved = OrderedJsonObject::new();
    for (name, value) in values.into_iter().flatten() {
        let value = match value {
            ChatTemplateKwargValue::String(value) => Some(value.clone().into()),
            ChatTemplateKwargValue::Number(value) => Some(OrderedJsonValue::from(
                serde_json::Value::Number(value.clone()),
            )),
            ChatTemplateKwargValue::Boolean(value) => Some((*value).into()),
            ChatTemplateKwargValue::Null => Some(OrderedJsonValue::Null),
            ChatTemplateKwargValue::Variable(variable) => {
                if !enabled && variable.omit_when_off.unwrap_or(false) {
                    None
                } else {
                    match variable.variable {
                        crate::ChatTemplateVariableName::ThinkingEnabled => Some(enabled.into()),
                        crate::ChatTemplateVariableName::ThinkingEffort if enabled => {
                            effort.map(OrderedJsonValue::from)
                        }
                        crate::ChatTemplateVariableName::ThinkingEffort => {
                            model_off_effort(model).map(OrderedJsonValue::from)
                        }
                        crate::ChatTemplateVariableName::ThinkingBudget => {
                            thinking_budget.map(OrderedJsonValue::from)
                        }
                    }
                }
            }
        };
        if let Some(value) = value {
            resolved.insert(name, value);
        }
    }
    Ok(resolved)
}

/// Encodes one already-projected canonical context into the ordered Chat
/// Completions wire request used for byte comparison in §10.8.
pub fn encode_openai_completions(
    context: EncodeContext<'_, OpenAiCompletions>,
    options: &OpenAiCompletionsOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let tool_plan = plan_openai_completions_tools(context.context, context.compat)?;
    let mut messages = convert_openai_completions_messages(
        context.model,
        context.context,
        context.compat,
        &tool_plan.grammar_input_properties,
    )?;
    let mut tools = tool_plan.tools;
    let cache_control = cache_control(context.compat, options.cache_retention);
    if let Some(cache_control) = cache_control.as_ref() {
        apply_cache_control(&mut messages, tools.as_mut(), cache_control);
    }

    let mut params = OrderedJsonObject::new();
    params.insert("model", context.model.common.model_ref.model.as_str());
    params.insert("messages", messages);
    params.insert("stream", true);

    let supports_long = context
        .compat
        .supports_long_cache_retention
        .unwrap_or(false);
    let uses_openai_cache_key = context
        .model
        .common
        .base_url
        .as_str()
        .contains("api.openai.com")
        && options.cache_retention != CacheRetention::None;
    if (uses_openai_cache_key || (options.cache_retention == CacheRetention::Long && supports_long))
        && let Some(session_id) = clamp_openai_prompt_cache_key(options.session_id.as_deref())
    {
        params.insert("prompt_cache_key", session_id);
    }
    if options.cache_retention == CacheRetention::Long && supports_long {
        params.insert("prompt_cache_retention", "24h");
    }
    if context.compat.supports_usage_in_streaming.unwrap_or(true) {
        params.insert(
            "stream_options",
            OrderedJsonObject::from_iter([("include_usage", true)]),
        );
    }
    if context.compat.supports_store.unwrap_or(false) {
        params.insert("store", false);
    }
    if let Some(max_tokens) = options.max_tokens.filter(|max_tokens| *max_tokens != 0) {
        params.insert(
            match options.max_tokens_field {
                MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
                MaxTokensField::MaxTokens => "max_tokens",
            },
            max_tokens,
        );
    }
    if let Some(temperature) = options.temperature {
        params.insert("temperature", temperature);
    }
    if let Some(tools) = tools {
        let has_active_tools = !tools.is_empty();
        params.insert("tools", tools);
        if has_active_tools && context.compat.zai_tool_stream.unwrap_or(false) {
            params.insert("tool_stream", true);
        }
    }
    if let Some(tool_choice) = options.tool_choice.as_ref() {
        params.insert("tool_choice", encode_openai_tool_choice(tool_choice));
    }
    encode_reasoning(
        &mut params,
        context.model,
        context.compat,
        &options.reasoning,
    );
    if let Some(routing) = context.model.config.compat.open_router_routing.as_ref()
        && let Some(value) = nonempty_serialized_object(routing)
    {
        params.insert("provider", value);
    }
    if let Some(routing) = context.model.config.compat.vercel_gateway_routing.as_ref()
        && (routing.only.is_some() || routing.order.is_some())
    {
        let mut gateway = OrderedJsonObject::new();
        if let Some(only) = &routing.only {
            gateway.insert(
                "only",
                only.iter()
                    .map(String::as_str)
                    .collect::<OrderedJsonArray>(),
            );
        }
        if let Some(order) = &routing.order {
            gateway.insert(
                "order",
                order
                    .iter()
                    .map(String::as_str)
                    .collect::<OrderedJsonArray>(),
            );
        }
        params.insert(
            "providerOptions",
            OrderedJsonObject::from_iter([("gateway", gateway)]),
        );
    }
    for (name, value) in &options.sampling {
        params.insert(name.clone(), value.clone());
    }
    Ok(params)
}

fn encode_openai_tool_choice(choice: &OpenAiCompletionsToolChoice) -> OrderedJsonValue {
    match choice {
        OpenAiCompletionsToolChoice::Auto => "auto".into(),
        OpenAiCompletionsToolChoice::None => "none".into(),
        OpenAiCompletionsToolChoice::Required => "required".into(),
        OpenAiCompletionsToolChoice::Function { name } => OrderedJsonObject::from_iter([
            ("type", OrderedJsonValue::from("function")),
            (
                "function",
                OrderedJsonObject::from_iter([("name", OrderedJsonValue::from(name.as_str()))])
                    .into(),
            ),
        ])
        .into(),
        OpenAiCompletionsToolChoice::Custom { name } => OrderedJsonObject::from_iter([
            ("type", OrderedJsonValue::from("custom")),
            (
                "custom",
                OrderedJsonObject::from_iter([("name", OrderedJsonValue::from(name.as_str()))])
                    .into(),
            ),
        ])
        .into(),
        OpenAiCompletionsToolChoice::AllowedTools { mode, tools } => {
            OrderedJsonObject::from_iter([
                ("type", OrderedJsonValue::from("allowed_tools")),
                (
                    "allowed_tools",
                    OrderedJsonObject::from_iter([
                        (
                            "mode",
                            OrderedJsonValue::from(match mode {
                                OpenAiAllowedToolsMode::Auto => "auto",
                                OpenAiAllowedToolsMode::Required => "required",
                            }),
                        ),
                        ("tools", OrderedJsonValue::from(tools.clone())),
                    ])
                    .into(),
                ),
            ])
            .into()
        }
    }
}

fn convert_openai_completions_messages(
    model: &TypedModelDescriptor<OpenAiCompletions>,
    context: &Context,
    compat: &OpenAiCompletionsCompat,
    grammar_input_properties: &std::collections::BTreeMap<String, String>,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut messages = OrderedJsonArray::new();
    if let Some(system_prompt) = context.system_prompt.as_deref() {
        let role = if model.common.reasoning && compat.supports_developer_role.unwrap_or(false) {
            "developer"
        } else {
            "system"
        };
        messages.push(OrderedJsonObject::from_iter([
            ("role", OrderedJsonValue::from(role)),
            ("content", OrderedJsonValue::from(system_prompt)),
        ]));
    }

    let mut index = 0;
    let mut last_role: Option<&str> = None;
    while index < context.messages.len() {
        match &context.messages[index] {
            crate::Message::User(message) => {
                if compat.requires_assistant_after_tool_result.unwrap_or(false)
                    && last_role == Some("tool_result")
                {
                    messages.push(assistant_bridge());
                }
                if let Some(content) = encode_user_content(&message.content) {
                    messages.push(OrderedJsonObject::from_iter([
                        ("role", OrderedJsonValue::from("user")),
                        ("content", content),
                    ]));
                    last_role = Some("user");
                }
                index += 1;
            }
            crate::Message::Assistant(message) => {
                if let Some(encoded) =
                    encode_assistant_message(model, message, compat, grammar_input_properties)?
                {
                    messages.push(encoded);
                    last_role = Some("assistant");
                }
                index += 1;
            }
            crate::Message::ToolResult(_) => {
                let mut image_parts = OrderedJsonArray::new();
                let mut deferred_tool_names = Vec::new();
                while index < context.messages.len() {
                    let crate::Message::ToolResult(message) = &context.messages[index] else {
                        break;
                    };
                    let mut text = Vec::new();
                    let mut has_images = false;
                    for block in &message.content {
                        match block {
                            ToolResultContent::Text { text: value, .. } => {
                                text.push(value.as_str())
                            }
                            ToolResultContent::Image {
                                data, mime_type, ..
                            } => {
                                has_images = true;
                                if model
                                    .common
                                    .modalities
                                    .input
                                    .contains(&crate::Modality::Image)
                                {
                                    image_parts.push(image_part(data, mime_type));
                                }
                            }
                        }
                    }
                    let content = if !text.is_empty() {
                        text.join("\n")
                    } else if has_images {
                        "(see attached image)".into()
                    } else {
                        "(no tool output)".into()
                    };
                    let mut tool = OrderedJsonObject::new();
                    tool.insert("role", "tool");
                    tool.insert("content", content);
                    tool.insert("tool_call_id", message.tool_call_id.as_str());
                    if compat.requires_tool_result_name.unwrap_or(false)
                        && !message.tool_name.is_empty()
                    {
                        tool.insert("name", message.tool_name.as_str());
                    }
                    messages.push(tool);
                    if compat.deferred_tools_mode == Some(crate::DeferredToolsMode::Kimi) {
                        for name in &message.added_tool_names {
                            if !deferred_tool_names.contains(name) {
                                deferred_tool_names.push(name.clone());
                            }
                        }
                    }
                    index += 1;
                }
                if !image_parts.is_empty() {
                    if compat.requires_assistant_after_tool_result.unwrap_or(false) {
                        messages.push(assistant_bridge());
                    }
                    let mut content = OrderedJsonArray::new();
                    content.push(text_part("Attached image(s) from tool result:"));
                    for part in image_parts {
                        content.push(part);
                    }
                    messages.push(OrderedJsonObject::from_iter([
                        ("role", OrderedJsonValue::from("user")),
                        ("content", OrderedJsonValue::from(content)),
                    ]));
                    last_role = Some("user");
                } else {
                    last_role = Some("tool_result");
                }
                if !deferred_tool_names.is_empty() {
                    let deferred_tools = deferred_tool_names
                        .iter()
                        .filter_map(|name| context.tools.iter().find(|tool| tool.name == *name))
                        .collect::<Vec<_>>();
                    if !deferred_tools.is_empty() {
                        messages.push(OrderedJsonObject::from_iter([
                            ("role", OrderedJsonValue::from("system")),
                            (
                                "tools",
                                encode_openai_tool_definitions(&deferred_tools, compat)?.into(),
                            ),
                        ]));
                    }
                }
            }
        }
    }
    Ok(messages)
}

fn encode_user_content(content: &[ContentBlock]) -> Option<OrderedJsonValue> {
    if let [ContentBlock::Text { text, .. }] = content {
        return Some(text.clone().into());
    }
    let mut parts = OrderedJsonArray::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => parts.push(text_part(text)),
            ContentBlock::Image {
                data, mime_type, ..
            } => parts.push(image_part(data, mime_type)),
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => {}
        }
    }
    (!parts.is_empty()).then_some(parts.into())
}

fn encode_assistant_message(
    model: &TypedModelDescriptor<OpenAiCompletions>,
    message: &AssistantMessage,
    compat: &OpenAiCompletionsCompat,
    grammar_input_properties: &std::collections::BTreeMap<String, String>,
) -> Result<Option<OrderedJsonObject>, EncodeError> {
    let mut encoded = OrderedJsonObject::new();
    encoded.insert("role", "assistant");
    encoded.insert(
        "content",
        if compat.requires_assistant_after_tool_result.unwrap_or(false) {
            OrderedJsonValue::from("")
        } else {
            OrderedJsonValue::Null
        },
    );

    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let thinking = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { id, text, .. } if !text.trim().is_empty() => {
                Some((id, text.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let details = collect_reasoning_details(message)?;
    if !thinking.is_empty() && compat.requires_thinking_as_text.unwrap_or(false) {
        let mut parts = OrderedJsonArray::new();
        parts.push(text_part(
            &thinking
                .iter()
                .map(|(_, value)| *value)
                .collect::<Vec<_>>()
                .join("\n\n"),
        ));
        for block in &message.content {
            if let ContentBlock::Text { text, .. } = block
                && !text.trim().is_empty()
            {
                parts.push(text_part(text));
            }
        }
        encoded.insert("content", parts);
    } else {
        if !text.is_empty() {
            encoded.insert("content", text);
        }
        if details.is_none()
            && let Some((block_id, _)) = thinking.first()
            && let Some(field) = reasoning_field_for_block(message, block_id)
        {
            encoded.insert(
                field,
                thinking
                    .iter()
                    .map(|(_, value)| *value)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    let mut tool_calls = OrderedJsonArray::new();
    for block in &message.content {
        let ContentBlock::ToolCall { call, .. } = block else {
            continue;
        };
        if let Some(property) = grammar_input_properties.get(&call.name) {
            let input = call
                .arguments
                .as_object()
                .and_then(|arguments| arguments.get(property))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| EncodeError::InvalidRequest {
                    message: format!(
                        "grammar tool call \"{}\" requires argument \"{}\" to be a string",
                        call.name, property
                    ),
                })?;
            tool_calls.push(OrderedJsonObject::from_iter([
                ("id", OrderedJsonValue::from(call.id.as_str())),
                ("type", OrderedJsonValue::from("custom")),
                (
                    "custom",
                    OrderedJsonObject::from_iter([
                        ("name", OrderedJsonValue::from(call.name.as_str())),
                        ("input", OrderedJsonValue::from(input)),
                    ])
                    .into(),
                ),
            ]));
            continue;
        }
        let arguments =
            crate::OrderedJsonWriter::stringify(&OrderedJsonValue::from(call.arguments.clone()))
                .map_err(|error| EncodeError::InvalidRequest {
                    message: format!("failed to encode tool arguments: {error}"),
                })?;
        tool_calls.push(OrderedJsonObject::from_iter([
            ("id", OrderedJsonValue::from(call.id.as_str())),
            ("type", OrderedJsonValue::from("function")),
            (
                "function",
                OrderedJsonObject::from_iter([
                    ("name", OrderedJsonValue::from(call.name.as_str())),
                    ("arguments", OrderedJsonValue::from(arguments)),
                ])
                .into(),
            ),
        ]));
    }
    if !tool_calls.is_empty() {
        encoded.insert("tool_calls", tool_calls);
    }
    if let Some(details) = details {
        encoded.insert("reasoning_details", details);
    }
    if compat
        .requires_reasoning_content_on_assistant_messages
        .unwrap_or(false)
        && model.common.reasoning
        && encoded.get("reasoning_content").is_none()
    {
        encoded.insert("reasoning_content", "");
    }

    let has_content = match encoded.get("content") {
        Some(OrderedJsonValue::String(value)) => !value.as_utf16().is_empty(),
        Some(OrderedJsonValue::Array(value)) => !value.is_empty(),
        _ => false,
    };
    Ok((has_content || encoded.get("tool_calls").is_some()).then_some(encoded))
}

fn collect_reasoning_details(
    message: &AssistantMessage,
) -> Result<Option<OrderedJsonArray>, EncodeError> {
    let target = ReplayScope::new(
        message.provider.clone(),
        message.api.clone(),
        message.requested_model.clone(),
        message
            .response_model
            .clone()
            .unwrap_or_else(|| message.requested_model.clone()),
    );
    for block in &message.content {
        let ContentBlock::Thinking { id, .. } = block else {
            continue;
        };
        let details =
            replay_details_for_target(message, &ReplayTarget::ContentBlock(id.clone()), &target)?;
        if !details.is_empty() {
            return Ok(Some(details));
        }
    }

    let mut legacy = OrderedJsonArray::new();
    for block in &message.content {
        let ContentBlock::ToolCall { call, .. } = block else {
            continue;
        };
        for detail in
            replay_details_for_target(message, &ReplayTarget::ToolCall(call.id.clone()), &target)?
        {
            legacy.push(detail);
        }
    }
    Ok((!legacy.is_empty()).then_some(legacy))
}

fn replay_details_for_target(
    message: &AssistantMessage,
    target: &ReplayTarget,
    request_scope: &ReplayScope,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut items = message
        .replay
        .items
        .iter()
        .filter(|item| {
            &item.target == target
                && item.kind.as_str() == OPENAI_CHAT_REASONING_DETAIL_KIND
                && item.is_complete_and_applicable(&message.replay.source, request_scope)
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.ordinal);
    let mut details = OrderedJsonArray::new();
    for item in items {
        let bytes = item.json_bytes().map_err(|_| EncodeError::InvalidRequest {
            message: "OpenAI reasoning detail is not encoded as JSON bytes".into(),
        })?;
        let json = std::str::from_utf8(bytes).map_err(|error| EncodeError::InvalidRequest {
            message: format!("OpenAI reasoning detail is not UTF-8 JSON: {error}"),
        })?;
        let parsed = parse_ordered_json(json).map_err(|error| EncodeError::InvalidRequest {
            message: format!("invalid OpenAI reasoning detail replay JSON: {error}"),
        })?;
        details.push(parsed);
    }
    Ok(details)
}

fn reasoning_field_for_block<'a>(
    message: &'a AssistantMessage,
    block_id: &crate::ContentBlockId,
) -> Option<&'a str> {
    let target = ReplayScope::new(
        message.provider.clone(),
        message.api.clone(),
        message.requested_model.clone(),
        message
            .response_model
            .clone()
            .unwrap_or_else(|| message.requested_model.clone()),
    );
    message
        .replay
        .items_for_block(block_id)
        .filter(|item| {
            item.kind.as_str() == OPENAI_CHAT_REASONING_FIELD_KIND
                && item.is_complete_and_applicable(&message.replay.source, &target)
        })
        .min_by_key(|item| item.ordinal)
        .and_then(ReplayItem::as_utf8)
        .filter(|field| is_openai_reasoning_field(field))
}

struct OpenAiToolPlan {
    tools: Option<OrderedJsonArray>,
    grammar_input_properties: std::collections::BTreeMap<String, String>,
}

fn plan_openai_completions_tools(
    context: &Context,
    compat: &OpenAiCompletionsCompat,
) -> Result<OpenAiToolPlan, EncodeError> {
    let grammar_input_properties = openai_grammar_tool_input_properties(context, compat)?;

    let deferred = if compat.deferred_tools_mode == Some(crate::DeferredToolsMode::Kimi) {
        context
            .messages
            .iter()
            .filter_map(|message| match message {
                crate::Message::ToolResult(result) => Some(result.added_tool_names.as_slice()),
                crate::Message::User(_) | crate::Message::Assistant(_) => None,
            })
            .flatten()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>()
    } else {
        std::collections::HashSet::new()
    };
    let active_tools = context
        .tools
        .iter()
        .filter(|tool| !deferred.contains(tool.name.as_str()))
        .collect::<Vec<_>>();
    let has_history = context.messages.iter().any(|message| match message {
        crate::Message::ToolResult(_) => true,
        crate::Message::Assistant(message) => message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. })),
        crate::Message::User(_) => false,
    });
    let tools = if active_tools.is_empty() {
        has_history.then(OrderedJsonArray::new)
    } else {
        Some(encode_openai_tool_definitions(&active_tools, compat)?)
    };
    Ok(OpenAiToolPlan {
        tools,
        grammar_input_properties,
    })
}

/// Resolves the canonical argument property used by every enabled grammar
/// tool in one request. The response decoder consumes the same map as the
/// encoder so streamed custom input can be framed as append-only JSON.
pub fn openai_grammar_tool_input_properties(
    context: &Context,
    compat: &OpenAiCompletionsCompat,
) -> Result<std::collections::BTreeMap<String, String>, EncodeError> {
    let mut properties = std::collections::BTreeMap::new();
    for tool in &context.tools {
        if let Some(grammar) = resolve_grammar_tool(tool, compat)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

fn encode_openai_tool_definitions(
    tools: &[&crate::ToolSpec],
    compat: &OpenAiCompletionsCompat,
) -> Result<OrderedJsonArray, EncodeError> {
    let mut encoded = OrderedJsonArray::new();
    for tool in tools {
        if let Some(grammar) = resolve_grammar_tool(tool, compat)? {
            encoded.push(OrderedJsonObject::from_iter([
                ("type", OrderedJsonValue::from("custom")),
                (
                    "custom",
                    OrderedJsonObject::from_iter([
                        ("name", OrderedJsonValue::from(tool.name.as_str())),
                        (
                            "description",
                            OrderedJsonValue::from(tool.description.as_str()),
                        ),
                        (
                            "format",
                            OrderedJsonObject::from_iter([
                                ("type", OrderedJsonValue::from("grammar")),
                                (
                                    "grammar",
                                    OrderedJsonObject::from_iter([
                                        ("syntax", OrderedJsonValue::from(grammar.syntax)),
                                        (
                                            "definition",
                                            OrderedJsonValue::from(grammar.definition.as_str()),
                                        ),
                                    ])
                                    .into(),
                                ),
                            ])
                            .into(),
                        ),
                    ])
                    .into(),
                ),
            ]));
            continue;
        }

        let (parameters, strict) = resolve_strict_tool(tool, compat)?;
        let mut function = OrderedJsonObject::new();
        function.insert("name", tool.name.as_str());
        function.insert("description", tool.description.as_str());
        function.insert("parameters", OrderedJsonValue::from(parameters));
        if compat.supports_strict_mode.unwrap_or(true) {
            function.insert("strict", strict.unwrap_or(false));
        }
        encoded.push(OrderedJsonObject::from_iter([
            ("type", OrderedJsonValue::from("function")),
            ("function", function.into()),
        ]));
    }
    Ok(encoded)
}

struct ResolvedGrammarTool {
    syntax: &'static str,
    definition: String,
    input_property: String,
}

fn resolve_grammar_tool(
    tool: &crate::ToolSpec,
    compat: &OpenAiCompletionsCompat,
) -> Result<Option<ResolvedGrammarTool>, EncodeError> {
    let Some(crate::ConstrainedSampling::Config(crate::ConstrainedSamplingConfig::Grammar {
        variants,
    })) = &tool.constrained_sampling
    else {
        return Ok(None);
    };
    if !compat.supports_openai_grammar_tools.unwrap_or(false) {
        return Ok(None);
    }

    let lark = variants
        .get(&crate::GrammarFormat::OpenAiLark)
        .filter(|value| !value.trim().is_empty());
    let regex = variants
        .get(&crate::GrammarFormat::OpenAiRegex)
        .filter(|value| !value.trim().is_empty());
    let (syntax, definition) = lark
        .map(|definition| ("lark", definition))
        .or_else(|| regex.map(|definition| ("regex", definition)))
        .ok_or_else(|| EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided",
                tool.name
            ),
        })?;
    let input_property = infer_grammar_input_property(&tool.parameters).map_err(|message| {
        EncodeError::InvalidRequest {
            message: format!(
                "tool \"{}\" cannot use grammar constrained sampling: {message}",
                tool.name
            ),
        }
    })?;
    Ok(Some(ResolvedGrammarTool {
        syntax,
        definition: definition.clone(),
        input_property,
    }))
}

fn infer_grammar_input_property(schema: &serde_json::Value) -> Result<String, String> {
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

fn resolve_strict_tool(
    tool: &crate::ToolSpec,
    compat: &OpenAiCompletionsCompat,
) -> Result<(serde_json::Value, Option<bool>), EncodeError> {
    let Some(crate::ConstrainedSampling::Config(crate::ConstrainedSamplingConfig::JsonSchema {
        strict,
    })) = tool.constrained_sampling
    else {
        return Ok((tool.parameters.clone(), None));
    };
    if !compat.supports_strict_mode.unwrap_or(true) {
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

    match make_strict_json_schema(&tool.parameters) {
        Ok(parameters) => Ok((parameters, Some(true))),
        Err(_message) if strict == crate::JsonSchemaStrictMode::Prefer => {
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

pub(crate) fn make_strict_json_schema(
    schema: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut strict = schema.clone();
    make_json_schema_node_strict(&mut strict)?;
    if strict.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err("root schema must have type object".into());
    }
    Ok(strict)
}

fn make_json_schema_node_strict(schema: &mut serde_json::Value) -> Result<(), String> {
    const UNSUPPORTED: &[&str] = &[
        "$ref",
        "$defs",
        "definitions",
        "allOf",
        "oneOf",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
        "unevaluatedProperties",
        "propertyNames",
        "contains",
        "prefixItems",
        "not",
        "if",
        "then",
        "else",
    ];
    let object = schema
        .as_object_mut()
        .ok_or_else(|| "boolean schemas are unsupported".to_owned())?;
    if let Some(key) = UNSUPPORTED.iter().find(|key| object.contains_key(**key)) {
        return Err(format!("{key} schemas are unsupported"));
    }

    if let Some(any_of) = object.get_mut("anyOf") {
        let variants = any_of
            .as_array_mut()
            .filter(|variants| !variants.is_empty())
            .ok_or_else(|| "anyOf must contain at least one schema".to_owned())?;
        for variant in variants {
            if is_structured_schema(variant) {
                return Err("object and array unions are unsupported".into());
            }
            make_json_schema_node_strict(variant)?;
        }
    }

    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        make_json_schema_node_strict(items)?;
    }

    let is_object = object.get("type").and_then(serde_json::Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &serde_json::Value::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    if object
        .get("properties")
        .is_some_and(|properties| !properties.is_object())
    {
        return Err("object properties must be a schema map".into());
    }
    if object.get("required").is_some_and(|required| {
        required
            .as_array()
            .is_none_or(|values| values.iter().any(|value| !value.is_string()))
    }) {
        return Err("object required must be a string array".into());
    }

    let property_names = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let required = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect::<std::collections::HashSet<_>>();
    if required.iter().any(|name| !property_names.contains(name)) {
        return Err("required contains an unknown property".into());
    }
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for name in &property_names {
            let property = properties
                .get_mut(name)
                .expect("property name came from this object");
            make_json_schema_node_strict(property)?;
            if !required.contains(name) && !schema_allows_null(property) {
                let original = std::mem::take(property);
                *property = serde_json::json!({
                    "anyOf": [original, { "type": "null" }]
                });
            }
        }
    }
    object.insert(
        "required".into(),
        serde_json::Value::Array(
            property_names
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    object.insert(
        "additionalProperties".into(),
        serde_json::Value::Bool(false),
    );
    Ok(())
}

fn is_structured_schema(schema: &serde_json::Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let structured_type = match schema.get("type") {
        Some(serde_json::Value::String(value)) => matches!(value.as_str(), "object" | "array"),
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|value| matches!(value, "object" | "array")),
        _ => false,
    };
    structured_type || schema.contains_key("properties") || schema.contains_key("items")
}

fn schema_allows_null(schema: &serde_json::Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    schema.get("type").is_some_and(|value| match value {
        serde_json::Value::String(value) => value == "null",
        serde_json::Value::Array(values) => values.iter().any(|value| value == "null"),
        _ => false,
    }) || schema.get("const").is_some_and(serde_json::Value::is_null)
        || schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|values| values.iter().any(serde_json::Value::is_null))
        || schema
            .get("anyOf")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|variants| variants.iter().any(schema_allows_null))
}

fn encode_reasoning(
    params: &mut OrderedJsonObject,
    model: &TypedModelDescriptor<OpenAiCompletions>,
    compat: &OpenAiCompletionsCompat,
    reasoning: &OpenAiReasoningPlan,
) {
    let format = compat
        .thinking_format
        .unwrap_or(OpenAiThinkingFormat::OpenAi);
    match &reasoning.mode {
        OpenAiReasoningMode::Disabled => {
            let off = model
                .config
                .thinking_levels
                .off
                .as_ref()
                .and_then(|support| match support {
                    LevelSupport::Value(OpenAiThinkingValue::Effort(value)) => Some(value.as_str()),
                    LevelSupport::Disabled | LevelSupport::Value(OpenAiThinkingValue::Disabled) => {
                        Some("none")
                    }
                    LevelSupport::Unsupported
                    | LevelSupport::Value(OpenAiThinkingValue::TokenBudget(_)) => None,
                });
            match format {
                OpenAiThinkingFormat::Zai if model.common.reasoning => {
                    params.insert(
                        "thinking",
                        OrderedJsonObject::from_iter([("type", "disabled")]),
                    );
                }
                OpenAiThinkingFormat::Qwen if model.common.reasoning => {
                    params.insert("enable_thinking", false);
                }
                OpenAiThinkingFormat::QwenChatTemplate if model.common.reasoning => {
                    params.insert(
                        "chat_template_kwargs",
                        OrderedJsonObject::from_iter([
                            ("enable_thinking", OrderedJsonValue::from(false)),
                            ("preserve_thinking", OrderedJsonValue::from(true)),
                        ]),
                    );
                }
                OpenAiThinkingFormat::ChatTemplate if model.common.reasoning => {
                    if let Ok(kwargs) =
                        resolve_chat_template_values(model, compat, false, None, None)
                        && !kwargs.is_empty()
                    {
                        params.insert("chat_template_kwargs", kwargs);
                    }
                }
                OpenAiThinkingFormat::Baseten if model.common.reasoning => {
                    if let Ok(args) = resolve_chat_template_values(model, compat, false, None, None)
                        && !args.is_empty()
                    {
                        params.insert("chat_template_args", args);
                    }
                    if compat.supports_reasoning_effort.unwrap_or(false)
                        && let Some(off) = off
                    {
                        params.insert("reasoning_effort", off);
                    }
                }
                OpenAiThinkingFormat::OpenRouter
                    if model.common.reasoning
                        && model.config.thinking_levels.off != Some(LevelSupport::Unsupported) =>
                {
                    params.insert(
                        "reasoning",
                        OrderedJsonObject::from_iter([("effort", off.unwrap_or("none"))]),
                    );
                }
                OpenAiThinkingFormat::DeepSeek
                    if model.common.reasoning
                        && model.config.thinking_levels.off != Some(LevelSupport::Unsupported) =>
                {
                    params.insert(
                        "thinking",
                        OrderedJsonObject::from_iter([("type", "disabled")]),
                    );
                }
                OpenAiThinkingFormat::StringThinking
                    if model.common.reasoning
                        && model.config.thinking_levels.off != Some(LevelSupport::Unsupported) =>
                {
                    params.insert("thinking", off.unwrap_or("none"));
                }
                OpenAiThinkingFormat::Together if model.common.reasoning => {
                    params.insert(
                        "reasoning",
                        OrderedJsonObject::from_iter([("enabled", false)]),
                    );
                }
                _ if model.common.reasoning
                    && compat.supports_reasoning_effort.unwrap_or(false) =>
                {
                    if let Some(off) = off {
                        params.insert("reasoning_effort", off);
                    }
                }
                _ => {}
            }
        }
        OpenAiReasoningMode::ReasoningEffort { effort, provenance } => match format {
            OpenAiThinkingFormat::Zai if model.common.reasoning => {
                params.insert(
                    "thinking",
                    OrderedJsonObject::from_iter([
                        ("type", OrderedJsonValue::from("enabled")),
                        ("clear_thinking", OrderedJsonValue::from(false)),
                    ]),
                );
                if compat.supports_reasoning_effort.unwrap_or(false) {
                    params.insert("reasoning_effort", effort.as_str());
                }
            }
            OpenAiThinkingFormat::Qwen if model.common.reasoning => {
                params.insert("enable_thinking", true);
                if compat.supports_reasoning_effort.unwrap_or(false) {
                    params.insert("reasoning_effort", effort.as_str());
                }
            }
            OpenAiThinkingFormat::Together if model.common.reasoning => {
                params.insert(
                    "reasoning",
                    OrderedJsonObject::from_iter([("enabled", true)]),
                );
                if compat.supports_reasoning_effort.unwrap_or(false) {
                    params.insert("reasoning_effort", effort.as_str());
                }
            }
            OpenAiThinkingFormat::AntLing
                if model.common.reasoning
                    && *provenance == OpenAiReasoningEffortProvenance::ModelMapping =>
            {
                params.insert(
                    "reasoning",
                    OrderedJsonObject::from_iter([("effort", effort.as_str())]),
                );
            }
            _ if compat.supports_reasoning_effort.unwrap_or(false) => {
                params.insert("reasoning_effort", effort.as_str());
            }
            _ => {}
        },
        OpenAiReasoningMode::Enabled => match format {
            OpenAiThinkingFormat::Zai if model.common.reasoning => {
                params.insert(
                    "thinking",
                    OrderedJsonObject::from_iter([
                        ("type", OrderedJsonValue::from("enabled")),
                        ("clear_thinking", OrderedJsonValue::from(false)),
                    ]),
                );
            }
            OpenAiThinkingFormat::Qwen if model.common.reasoning => {
                params.insert("enable_thinking", true);
            }
            OpenAiThinkingFormat::QwenChatTemplate if model.common.reasoning => {
                params.insert(
                    "chat_template_kwargs",
                    OrderedJsonObject::from_iter([
                        ("enable_thinking", OrderedJsonValue::from(true)),
                        ("preserve_thinking", OrderedJsonValue::from(true)),
                    ]),
                );
            }
            OpenAiThinkingFormat::Together if model.common.reasoning => {
                params.insert(
                    "reasoning",
                    OrderedJsonObject::from_iter([("enabled", true)]),
                );
            }
            _ => {}
        },
        OpenAiReasoningMode::OpenRouter { effort } => {
            params.insert(
                "reasoning",
                OrderedJsonObject::from_iter([("effort", effort.as_str())]),
            );
        }
        OpenAiReasoningMode::DeepSeek { enabled, effort } => {
            params.insert(
                "thinking",
                OrderedJsonObject::from_iter([(
                    "type",
                    if *enabled { "enabled" } else { "disabled" },
                )]),
            );
            if let Some(effort) = effort {
                params.insert("reasoning_effort", effort.as_str());
            }
        }
        OpenAiReasoningMode::ChatTemplate {
            kwargs,
            reasoning_effort,
        } => {
            if !kwargs.is_empty() {
                params.insert(
                    if format == OpenAiThinkingFormat::Baseten {
                        "chat_template_args"
                    } else {
                        "chat_template_kwargs"
                    },
                    kwargs.clone(),
                );
            }
            if let Some(effort) = reasoning_effort {
                params.insert("reasoning_effort", effort.as_str());
            }
        }
        OpenAiReasoningMode::StringThinking { value } => {
            params.insert("thinking", value.as_str());
        }
    }
    if let Some(token_budget) = reasoning.token_budget {
        params.insert(
            thinking_budget_field_name(token_budget.field),
            token_budget.budget,
        );
    }
}

fn model_off_effort(model: &TypedModelDescriptor<OpenAiCompletions>) -> Option<&str> {
    match model.config.thinking_levels.off.as_ref() {
        Some(LevelSupport::Value(OpenAiThinkingValue::Effort(value))) => Some(value.as_str()),
        Some(LevelSupport::Disabled | LevelSupport::Value(OpenAiThinkingValue::Disabled)) => {
            Some("none")
        }
        Some(
            LevelSupport::Unsupported | LevelSupport::Value(OpenAiThinkingValue::TokenBudget(_)),
        )
        | None => None,
    }
}

fn thinking_budget_field_name(field: ThinkingTokenBudgetField) -> &'static str {
    match field {
        ThinkingTokenBudgetField::ThinkingTokenBudget => "thinking_token_budget",
        ThinkingTokenBudgetField::ThinkingBudget => "thinking_budget",
        ThinkingTokenBudgetField::ThinkingBudgetTokens => "thinking_budget_tokens",
    }
}

fn text_part(text: &str) -> OrderedJsonObject {
    OrderedJsonObject::from_iter([
        ("type", OrderedJsonValue::from("text")),
        ("text", OrderedJsonValue::from(text)),
    ])
}

fn image_part(data: &str, mime_type: &str) -> OrderedJsonObject {
    OrderedJsonObject::from_iter([
        ("type", OrderedJsonValue::from("image_url")),
        (
            "image_url",
            OrderedJsonObject::from_iter([(
                "url",
                OrderedJsonValue::from(format!("data:{mime_type};base64,{data}")),
            )])
            .into(),
        ),
    ])
}

fn assistant_bridge() -> OrderedJsonObject {
    OrderedJsonObject::from_iter([
        ("role", OrderedJsonValue::from("assistant")),
        (
            "content",
            OrderedJsonValue::from("I have processed the tool results."),
        ),
    ])
}

fn cache_control(
    compat: &OpenAiCompletionsCompat,
    retention: CacheRetention,
) -> Option<OrderedJsonObject> {
    if compat.cache_control_format != Some(CacheControlFormat::Anthropic)
        || retention == CacheRetention::None
    {
        return None;
    }
    let mut value = OrderedJsonObject::new();
    value.insert("type", "ephemeral");
    if retention == CacheRetention::Long && compat.supports_long_cache_retention.unwrap_or(false) {
        value.insert("ttl", "1h");
    }
    Some(value)
}

fn apply_cache_control(
    messages: &mut OrderedJsonArray,
    tools: Option<&mut OrderedJsonArray>,
    cache_control: &OrderedJsonObject,
) {
    for message in messages.as_mut_slice() {
        let OrderedJsonValue::Object(message) = message else {
            continue;
        };
        if ordered_member_equals(message, "role", "system")
            || ordered_member_equals(message, "role", "developer")
        {
            let _ = add_cache_control_to_content(message, cache_control);
            break;
        }
    }
    if let Some(tools) = tools
        && let Some(OrderedJsonValue::Object(tool)) = tools.as_mut_slice().last_mut()
    {
        tool.insert("cache_control", cache_control.clone());
    }
    for message in messages.as_mut_slice().iter_mut().rev() {
        let OrderedJsonValue::Object(message) = message else {
            continue;
        };
        if (ordered_member_equals(message, "role", "user")
            || ordered_member_equals(message, "role", "assistant")
            || ordered_member_equals(message, "role", "tool"))
            && add_cache_control_to_content(message, cache_control)
        {
            break;
        }
    }
}

fn add_cache_control_to_content(
    message: &mut OrderedJsonObject,
    cache_control: &OrderedJsonObject,
) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content {
        OrderedJsonValue::String(value) if !value.as_utf16().is_empty() => {
            let text = value.clone();
            *content = OrderedJsonArray::from_iter([OrderedJsonObject::from_iter([
                ("type", OrderedJsonValue::from("text")),
                ("text", OrderedJsonValue::String(text)),
                ("cache_control", cache_control.clone().into()),
            ])])
            .into();
            true
        }
        OrderedJsonValue::Array(parts) => {
            for part in parts.as_mut_slice().iter_mut().rev() {
                let OrderedJsonValue::Object(part) = part else {
                    continue;
                };
                if ordered_member_equals(part, "type", "text") {
                    part.insert("cache_control", cache_control.clone());
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn ordered_member_equals(object: &OrderedJsonObject, name: &str, expected: &str) -> bool {
    let Some(OrderedJsonValue::String(value)) = object.get(name) else {
        return false;
    };
    value.as_utf16().iter().copied().eq(expected.encode_utf16())
}

fn ordered_nonempty_string(value: Option<&OrderedJsonValue>) -> bool {
    matches!(value, Some(OrderedJsonValue::String(value)) if !value.as_utf16().is_empty())
}

fn nonempty_serialized_object<T: Serialize>(value: &T) -> Option<OrderedJsonObject> {
    match OrderedJsonValue::from(serde_json::to_value(value).ok()?) {
        OrderedJsonValue::Object(object) if !object.is_empty() => Some(object),
        _ => None,
    }
}

/// Applies Pi's 64-Unicode-code-point prompt-cache key limit.
pub fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|value| {
        value
            .chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect()
    })
}

/// Returns whether a string names one of the three replayable visible
/// reasoning fields accepted by pinned Pi.
pub fn is_openai_reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning" | "reasoning_content" | "reasoning_text")
}

/// Inserts API-family session-affinity defaults before explicit request
/// headers and the final Models-level header transform.
///
/// Pinned Pi creates these defaults before merging `options.headers`. Keeping
/// this operation in the control-plane header phase lets explicit headers
/// override or delete them and preserves the transform's final precedence.
pub(crate) fn apply_openai_completions_session_affinity_headers(
    effective_base_url: &Url,
    compat_overrides: &OpenAiCompletionsCompat,
    options: &SimpleGenerationOptions,
    headers: &mut HeaderMap,
) -> Result<(), MiddlewareError> {
    let compat = resolve_openai_completions_compat(effective_base_url, compat_overrides);
    if !compat.send_session_affinity_headers.unwrap_or(false)
        || options.cache_retention == Some(CacheRetention::None)
    {
        return Ok(());
    }
    let Some(session_id) = options.session_id.as_deref() else {
        return Ok(());
    };
    let session = HeaderValue::from_str(session_id).map_err(|error| {
        MiddlewareError::new(
            "invalid_header_value",
            format!("session ID cannot be encoded as a header: {error}"),
        )
    })?;
    let mut insert = |name: &'static str| {
        headers.insert(http::HeaderName::from_static(name), session.clone());
    };
    match compat
        .session_affinity_format
        .unwrap_or(SessionAffinityFormat::OpenAi)
    {
        SessionAffinityFormat::OpenRouter => insert("x-session-id"),
        SessionAffinityFormat::OpenAi => {
            insert("session_id");
            insert("x-client-request-id");
            insert("x-session-affinity");
        }
        SessionAffinityFormat::OpenAiNoSession => {
            insert("x-client-request-id");
            insert("x-session-affinity");
        }
    }
    Ok(())
}

/// Imports old Pi `ToolCall.thoughtSignature` encrypted-detail values as
/// replay items when no thinking-block detail already exists. New Rust
/// messages never write the legacy field.
pub fn import_legacy_openai_chat_tool_signatures<I>(
    message: &mut AssistantMessage,
    signatures: I,
) -> Result<usize, EncodeError>
where
    I: IntoIterator<Item = (ToolCallId, String)>,
{
    if message.replay.items.iter().any(|item| {
        matches!(item.target, ReplayTarget::ContentBlock(_))
            && item.kind.as_str() == OPENAI_CHAT_REASONING_DETAIL_KIND
    }) {
        return Ok(0);
    }
    let mut next_ordinal = message
        .replay
        .items
        .iter()
        .map(|item| item.ordinal)
        .max()
        .map_or(0, |ordinal| ordinal.saturating_add(1));
    let mut imported = 0;
    for (call_id, signature) in signatures {
        let Ok(detail) = parse_ordered_json(&signature) else {
            // Pi's legacy fallback treats malformed `thoughtSignature` values
            // as absent rather than rejecting the request.
            continue;
        };
        if !is_legacy_encrypted_detail(&detail) {
            continue;
        }
        let bytes = crate::OrderedJsonWriter::to_vec(&detail).map_err(|error| {
            EncodeError::InvalidRequest {
                message: format!("failed to normalize legacy OpenAI reasoning detail: {error}"),
            }
        })?;
        message.replay.items.push(ReplayItem {
            id: ReplayItemId::new(format!("legacy-openai-chat-{next_ordinal}")),
            ordinal: next_ordinal,
            target: ReplayTarget::ToolCall(call_id),
            kind: ReplayKind::new(OPENAI_CHAT_REASONING_DETAIL_KIND),
            applicability: ReplayApplicability::ExactProviderApiModel,
            completeness: ReplayCompleteness::Complete,
            payload: crate::OpaquePayload::JsonBytes(bytes),
        });
        next_ordinal = next_ordinal.saturating_add(1);
        imported += 1;
    }
    Ok(imported)
}

fn is_legacy_encrypted_detail(value: &OrderedJsonValue) -> bool {
    let OrderedJsonValue::Object(value) = value else {
        return false;
    };
    ordered_member_equals(value, "type", "reasoning.encrypted")
        && ordered_nonempty_string(value.get("id"))
        && ordered_nonempty_string(value.get("data"))
        && matches!(
            value.get("format"),
            None | Some(OrderedJsonValue::String(_))
        )
        && matches!(value.get("index"), None | Some(OrderedJsonValue::Number(_)))
}

/// OpenAI Chat Completions handoff hooks: recognize family replay values and
/// enforce Pi's target tool-call identifier rules.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiCompletionsHandoff;

impl ApiFamilyHandoff for OpenAiCompletionsHandoff {
    fn recognizes_replay_kind(&self, kind: &ReplayKind) -> bool {
        matches!(
            kind.as_str(),
            OPENAI_CHAT_REASONING_FIELD_KIND | OPENAI_CHAT_REASONING_DETAIL_KIND
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

impl ToolCallIdPolicy for OpenAiCompletionsHandoff {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        let id = original.as_str();
        if let Some(separator) = id.find('|') {
            let call = sanitize_tool_id_part(&id[..separator]);
            let item = sanitize_tool_id_part(&id[separator + 1..]);
            let combined = if item.is_empty() {
                call.clone()
            } else {
                format!("{call}_{item}")
            };
            if combined.len() <= 40 {
                return Ok(ToolCallId::new(combined));
            }
            let hash = short_hash(id);
            let hash = &hash[..hash.len().min(8)];
            let prefix_len = 40_usize.saturating_sub(hash.len() + 1).max(1);
            return Ok(ToolCallId::new(format!(
                "{}_{}",
                call.chars().take(prefix_len).collect::<String>(),
                hash
            )));
        }
        if target.provider.as_str() == "openai" && id.len() > 40 {
            return Ok(ToolCallId::new(id.chars().take(40).collect::<String>()));
        }
        Ok(original.clone())
    }
}

fn sanitize_tool_id_part(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
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
