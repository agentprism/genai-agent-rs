//! Mistral Conversations lowering and ordered wire encoding.
//!
//! Behavior follows pinned Pi `api/mistral-conversations.ts`; the typed family
//! realizes architecture v2 part 2 §5.1 and §10.8.

use crate::{
    ApiFamily, ApiFamilyHandoff, AssistantFinishReason, CacheRetention, ConstrainedSampling,
    ConstrainedSamplingConfig, ContentBlock, Context, EncodeContext, EncodeError, HandoffError,
    HandoffReport, LevelSupport, Message, MistralModelConfig, ModelFingerprint, ModelId,
    OrderedJsonArray, OrderedJsonObject, OrderedJsonValue, ReasoningLevel, ReplayKind,
    SimpleGenerationOptions, SimpleLoweringContext, ToolCallId, ToolCallIdPolicy, ToolChoice,
    ToolResultContent, ToolSpec, TypedModelDescriptor, make_strict_json_schema, trim_ecmascript,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use url::Url;

/// Mistral Conversations API-family marker.
#[derive(Clone, Copy, Debug, Default)]
pub struct MistralConversations;

/// Stateful Mistral tool-call identifier policy.
///
/// A fresh value must be used for each context projection so ownership of a
/// normalized identifier follows transcript and content-block encounter order,
/// matching pinned Pi's request-scoped normalizer.
#[derive(Debug, Default)]
pub struct MistralToolCallIdPolicy {
    state: Mutex<MistralToolCallIdState>,
}

#[derive(Debug, Default)]
struct MistralToolCallIdState {
    normalized_by_original: BTreeMap<ToolCallId, ToolCallId>,
    original_by_normalized: BTreeMap<ToolCallId, ToolCallId>,
}

impl ToolCallIdPolicy for MistralToolCallIdPolicy {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(normalized) = state.normalized_by_original.get(original) {
            return Ok(normalized.clone());
        }

        let mut attempt = 0;
        loop {
            let candidate =
                ToolCallId::new(derive_mistral_tool_call_id(original.as_str(), attempt));
            if state
                .original_by_normalized
                .get(&candidate)
                .is_none_or(|owner| owner == original)
            {
                state
                    .original_by_normalized
                    .insert(candidate.clone(), original.clone());
                state
                    .normalized_by_original
                    .insert(original.clone(), candidate.clone());
                return Ok(candidate);
            }
            attempt += 1;
        }
    }
}

/// Mistral Conversations replay and handoff hooks.
#[derive(Debug, Default)]
pub struct MistralConversationsHandoff {
    tool_call_ids: MistralToolCallIdPolicy,
}

impl ApiFamilyHandoff for MistralConversationsHandoff {
    fn recognizes_replay_kind(&self, _kind: &ReplayKind) -> bool {
        false
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        &self.tool_call_ids
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

/// Mistral currently has no endpoint-detected compatibility flags.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MistralCompat;

/// Native Mistral tool-selection shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MistralToolChoice {
    /// Let the model decide.
    Auto,
    /// Disable tools.
    None,
    /// Require any tool using Mistral's native spelling.
    Any,
    /// Require a tool call.
    Required,
    /// Require one named function.
    Function {
        /// Function name.
        name: String,
    },
}

/// Fully lowered Mistral request options.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MistralOptions {
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Native tool selection.
    pub tool_choice: Option<MistralToolChoice>,
    /// Native prompt-mode reasoning switch.
    pub prompt_mode: Option<String>,
    /// Native effort value (`none` or `high`).
    pub reasoning_effort: Option<String>,
    /// Cache retention selection.
    pub cache_retention: Option<CacheRetention>,
    /// Prompt-cache/session affinity key.
    pub session_id: Option<String>,
}

/// Typed Mistral simple-options patch.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MistralSimplePatch {
    /// Native tool choice overriding the common `auto`/`none` pair.
    pub tool_choice: Option<MistralToolChoice>,
}

impl ApiFamily for MistralConversations {
    const API_ID: &'static str = "mistral-conversations";

    type Compat = MistralCompat;
    type ModelConfig = MistralModelConfig;
    type FullOptions = MistralOptions;
    type OptionsPatch = MistralSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, crate::LoweringError> {
        Ok(model_overrides.clone())
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, crate::LoweringError> {
        let maximum = if context.model.common.limits.context_window == 0 {
            simple
                .max_output_tokens
                .unwrap_or(context.model.common.limits.max_output_tokens)
                .max(1)
        } else {
            simple
                .max_output_tokens
                .unwrap_or(context.model.common.limits.max_output_tokens)
                .min(u32::try_from(context.available_context_tokens.max(1)).unwrap_or(u32::MAX))
        };
        let tool_choice = patch.tool_choice.clone().or_else(|| {
            simple.tool_choice.map(|choice| match choice {
                ToolChoice::Auto => MistralToolChoice::Auto,
                ToolChoice::None => MistralToolChoice::None,
            })
        });
        let reasoning = simple
            .reasoning
            .map(|level| clamp_mistral_level(context.model, level, simple.reasoning_fallback))
            .transpose()?;
        let reasoning_enabled = context.model.common.reasoning
            && reasoning.is_some_and(|level| level != ReasoningLevel::Off);
        let uses_effort = uses_reasoning_effort(&context.model.common.model_ref.model);
        let reasoning_effort = reasoning_enabled.then(|| {
            let level = reasoning.expect("reasoning-enabled requires a level");
            match context.model.config.thinking_levels.get(level) {
                Some(LevelSupport::Value(value)) => value.clone(),
                Some(LevelSupport::Disabled) => "none".into(),
                Some(LevelSupport::Unsupported) | None => "high".into(),
            }
        });
        Ok(MistralOptions {
            temperature: simple.temperature,
            max_tokens: Some(maximum),
            tool_choice,
            prompt_mode: (reasoning_enabled && !uses_effort).then(|| "reasoning".into()),
            reasoning_effort: (reasoning_enabled && uses_effort)
                .then(|| reasoning_effort.unwrap_or_else(|| "high".into())),
            cache_retention: simple.cache_retention,
            session_id: simple.session_id.clone(),
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_mistral_conversations(context.model, context.context, options)
    }
}

fn clamp_mistral_level(
    model: &TypedModelDescriptor<MistralConversations>,
    requested: ReasoningLevel,
    fallback: crate::ReasoningFallback,
) -> Result<ReasoningLevel, crate::LoweringError> {
    let supported = |level| match model.config.thinking_levels.get(level) {
        Some(LevelSupport::Unsupported) => false,
        Some(LevelSupport::Disabled | LevelSupport::Value(_)) => true,
        None => !matches!(level, ReasoningLevel::Xhigh | ReasoningLevel::Max),
    };
    if supported(requested) {
        return Ok(requested);
    }
    if matches!(fallback, crate::ReasoningFallback::Strict) {
        return Err(crate::LoweringError::UnsupportedReasoningLevel { requested });
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

fn uses_reasoning_effort(model: &ModelId) -> bool {
    matches!(
        model.as_str(),
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

/// Encodes the ordered Mistral Chat Completions request body.
pub fn encode_mistral_conversations(
    model: &TypedModelDescriptor<MistralConversations>,
    context: &crate::Context,
    options: &MistralOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let mut messages = OrderedJsonArray::new();
    if let Some(system) = context
        .system_prompt
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let mut message = OrderedJsonObject::new();
        message.insert("role", "system");
        message.insert("content", system);
        messages.push(message);
    }
    let supports_images = model
        .common
        .modalities
        .input
        .contains(&crate::Modality::Image);
    for message in &context.messages {
        match message {
            Message::User(message) => {
                if let [ContentBlock::Text { text, .. }] = message.content.as_slice() {
                    let mut wire = OrderedJsonObject::new();
                    wire.insert("role", "user");
                    wire.insert("content", text.as_str());
                    messages.push(wire);
                    continue;
                }
                let had_images = message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }));
                let mut content = OrderedJsonArray::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            let mut chunk = OrderedJsonObject::new();
                            chunk.insert("type", "text");
                            chunk.insert("text", text.as_str());
                            content.push(chunk);
                        }
                        ContentBlock::Image {
                            data, mime_type, ..
                        } if supports_images => {
                            let mut chunk = OrderedJsonObject::new();
                            chunk.insert("type", "image_url");
                            chunk.insert("image_url", format!("data:{mime_type};base64,{data}"));
                            content.push(chunk);
                        }
                        ContentBlock::Image { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::ToolCall { .. } => {}
                    }
                }
                let mut wire = OrderedJsonObject::new();
                wire.insert("role", "user");
                if !content.is_empty() {
                    wire.insert("content", content);
                } else if had_images && !supports_images {
                    wire.insert("content", "(image omitted: model does not support images)");
                } else {
                    continue;
                }
                messages.push(wire);
            }
            Message::Assistant(message)
                if !matches!(
                    message.finish.reason,
                    AssistantFinishReason::Error | AssistantFinishReason::Aborted
                ) =>
            {
                let mut content = OrderedJsonArray::new();
                let mut calls = OrderedJsonArray::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text, .. } if !trim_ecmascript(text).is_empty() => {
                            let mut chunk = OrderedJsonObject::new();
                            chunk.insert("type", "text");
                            chunk.insert("text", text.as_str());
                            content.push(chunk);
                        }
                        ContentBlock::Thinking { text, .. }
                            if !trim_ecmascript(text).is_empty() =>
                        {
                            let mut thought = OrderedJsonObject::new();
                            thought.insert("type", "text");
                            thought.insert("text", text.as_str());
                            let mut chunk = OrderedJsonObject::new();
                            chunk.insert("type", "thinking");
                            chunk.insert(
                                "thinking",
                                OrderedJsonArray::from_iter([OrderedJsonValue::from(thought)]),
                            );
                            content.push(chunk);
                        }
                        ContentBlock::ToolCall { call, .. } => {
                            let mut function = OrderedJsonObject::new();
                            function.insert("name", call.name.as_str());
                            function.insert(
                                "arguments",
                                crate::OrderedJsonWriter::stringify(&OrderedJsonValue::from(
                                    call.arguments.clone(),
                                ))
                                .map_err(|error| invalid(model, error.to_string()))?,
                            );
                            let mut tool_call = OrderedJsonObject::new();
                            tool_call.insert("id", call.id.as_str());
                            tool_call.insert("type", "function");
                            tool_call.insert("function", function);
                            tool_call.insert("index", 0);
                            calls.push(tool_call);
                        }
                        ContentBlock::Text { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::Image { .. } => {}
                    }
                }
                if content.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut wire = OrderedJsonObject::new();
                wire.insert("role", "assistant");
                wire.insert("prefix", false);
                if !content.is_empty() {
                    wire.insert("content", content);
                }
                if !calls.is_empty() {
                    wire.insert("tool_calls", calls);
                }
                messages.push(wire);
            }
            Message::ToolResult(result) => {
                let has_images = result
                    .content
                    .iter()
                    .any(|block| matches!(block, ToolResultContent::Image { .. }));
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text { text, .. } => Some(text.as_str()),
                        ToolResultContent::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut content = OrderedJsonArray::new();
                let mut text_chunk = OrderedJsonObject::new();
                text_chunk.insert("type", "text");
                text_chunk.insert(
                    "text",
                    tool_result_text(&text, has_images, supports_images, result.is_error),
                );
                content.push(text_chunk);
                if supports_images {
                    for block in &result.content {
                        if let ToolResultContent::Image {
                            data, mime_type, ..
                        } = block
                        {
                            let mut chunk = OrderedJsonObject::new();
                            chunk.insert("type", "image_url");
                            chunk.insert("image_url", format!("data:{mime_type};base64,{data}"));
                            content.push(chunk);
                        }
                    }
                }
                let mut wire = OrderedJsonObject::new();
                wire.insert("role", "tool");
                wire.insert("name", result.tool_name.as_str());
                wire.insert("content", content);
                wire.insert("tool_call_id", result.tool_call_id.as_str());
                messages.push(wire);
            }
            Message::Assistant(_) => {}
        }
    }

    let mut request = OrderedJsonObject::new();
    request.insert("model", model.common.model_ref.model.as_str());
    request.insert("stream", true);
    request.insert("messages", messages);
    if !context.tools.is_empty() {
        request.insert("tools", encode_tools(model, &context.tools)?);
    }
    if let Some(temperature) = options.temperature {
        request.insert("temperature", temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        request.insert("max_tokens", max_tokens);
    }
    if let Some(choice) = &options.tool_choice {
        request.insert("tool_choice", encode_tool_choice(choice));
    }
    if let Some(prompt_mode) = &options.prompt_mode {
        request.insert("prompt_mode", prompt_mode.as_str());
    }
    if let Some(effort) = &options.reasoning_effort {
        request.insert("reasoning_effort", effort.as_str());
    }
    if options.cache_retention != Some(CacheRetention::None)
        && let Some(session_id) = options
            .session_id
            .as_deref()
            .filter(|value| !value.is_empty())
    {
        request.insert("prompt_cache_key", session_id);
    }
    Ok(request)
}

fn encode_tools(
    model: &TypedModelDescriptor<MistralConversations>,
    tools: &[ToolSpec],
) -> Result<OrderedJsonArray, EncodeError> {
    tools
        .iter()
        .map(|tool| {
            let (parameters, strict) = match &tool.constrained_sampling {
                Some(ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema {
                    strict,
                })) => match make_strict_json_schema(&tool.parameters) {
                    Ok(parameters) => (parameters, true),
                    Err(message) if matches!(strict, crate::JsonSchemaStrictMode::Require) => {
                        return Err(invalid(model, message));
                    }
                    Err(_) => (tool.parameters.clone(), false),
                },
                _ => (tool.parameters.clone(), false),
            };
            let mut function = OrderedJsonObject::new();
            function.insert("name", tool.name.as_str());
            function.insert("description", tool.description.as_str());
            function.insert("parameters", OrderedJsonValue::from(parameters));
            function.insert("strict", strict);
            let mut wire = OrderedJsonObject::new();
            wire.insert("type", "function");
            wire.insert("function", function);
            Ok(OrderedJsonValue::from(wire))
        })
        .collect()
}

fn encode_tool_choice(choice: &MistralToolChoice) -> OrderedJsonValue {
    match choice {
        MistralToolChoice::Auto => "auto".into(),
        MistralToolChoice::None => "none".into(),
        MistralToolChoice::Any => "any".into(),
        MistralToolChoice::Required => "required".into(),
        MistralToolChoice::Function { name } => {
            let mut function = OrderedJsonObject::new();
            function.insert("name", name.as_str());
            let mut value = OrderedJsonObject::new();
            value.insert("type", "function");
            value.insert("function", function);
            value.into()
        }
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: usize) -> String {
    let normalized = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    if attempt == 0 && normalized.len() == 9 {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed_base.to_owned()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed).chars().take(9).collect()
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

fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let trimmed = trim_ecmascript(text);
    let prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{prefix}{trimmed}{suffix}");
    }
    match (has_images, supports_images, is_error) {
        (true, true, true) => "[tool error] (see attached image)".into(),
        (true, true, false) => "(see attached image)".into(),
        (true, false, true) => "[tool error] (image omitted: model does not support images)".into(),
        (true, false, false) => "(image omitted: model does not support images)".into(),
        (false, _, true) => "[tool error] (no tool output)".into(),
        (false, _, false) => "(no tool output)".into(),
    }
}

fn invalid(
    _model: &TypedModelDescriptor<MistralConversations>,
    message: impl Into<String>,
) -> EncodeError {
    EncodeError::InvalidRequest {
        message: message.into(),
    }
}
