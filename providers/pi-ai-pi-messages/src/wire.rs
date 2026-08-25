//! Ordered pi-messages request lowering and legacy-context projection.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use pi_ai::{
    ApiFamily, AssistantFinishReason, CacheRetention, ContentBlock, CustomApiModelConfig,
    EncodeContext, EncodeError, Message, OpaquePayload, OrderedJsonArray, OrderedJsonObject,
    OrderedJsonValue, ReasoningLevel, ReplayCompleteness, ReplayTarget, SimpleGenerationOptions,
    SimpleLoweringContext, ToolChoice, ToolResultContent, parse_ordered_json,
};
use serde::{Deserialize, Serialize};
use url::Url;

/// pi-messages API-family marker.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiMessages;

/// pi-messages has no endpoint-detected compatibility switches.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PiMessagesCompat;

/// Native pi-messages tool selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiMessagesToolChoice {
    /// Let the upstream provider choose.
    Auto,
    /// Disable tool use.
    None,
    /// Require some tool.
    Required,
    /// Require one named function.
    Function {
        /// Function name.
        name: String,
    },
}

/// Fully lowered pi-messages request options.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiMessagesOptions {
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Requested reasoning level.
    pub reasoning: Option<ReasoningLevel>,
    /// Gateway prompt-cache retention.
    pub cache_retention: Option<CacheRetention>,
    /// Provider session identifier.
    pub session_id: Option<String>,
    /// Tool selection.
    pub tool_choice: Option<PiMessagesToolChoice>,
    /// Ask the gateway for routing diagnostics.
    pub debug: bool,
}

/// Typed pi-messages patch for options absent from the common surface.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiMessagesSimplePatch {
    /// Native tool selection overriding the common pair.
    pub tool_choice: Option<PiMessagesToolChoice>,
    /// Ask the gateway for routing diagnostics.
    pub debug: bool,
}

impl ApiFamily for PiMessages {
    const API_ID: &'static str = "pi-messages";

    type Compat = PiMessagesCompat;
    type ModelConfig = CustomApiModelConfig;
    type FullOptions = PiMessagesOptions;
    type OptionsPatch = PiMessagesSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        _effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, pi_ai::LoweringError> {
        Ok(model_overrides.clone())
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, pi_ai::LoweringError> {
        let _ = context;
        Ok(PiMessagesOptions {
            temperature: simple.temperature,
            // Pinned pi-messages forwards the caller's simple maxTokens
            // unchanged. The gateway owns any downstream provider clamp.
            max_tokens: simple.max_output_tokens,
            reasoning: simple.reasoning,
            cache_retention: simple.cache_retention,
            session_id: simple.session_id.clone(),
            tool_choice: patch.tool_choice.clone().or_else(|| {
                simple.tool_choice.map(|choice| match choice {
                    ToolChoice::Auto => PiMessagesToolChoice::Auto,
                    ToolChoice::None => PiMessagesToolChoice::None,
                })
            }),
            debug: patch.debug,
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        encode_pi_messages(
            context.model.common.model_ref.model.as_str(),
            context.context,
            options,
        )
    }
}

/// Encodes Pi's exact `{ model, context, options }` insertion order.
pub fn encode_pi_messages(
    model: &str,
    context: &pi_ai::Context,
    options: &PiMessagesOptions,
) -> Result<OrderedJsonObject, EncodeError> {
    let mut body = OrderedJsonObject::new();
    body.insert("model", model);
    body.insert("context", legacy_context(context)?);
    body.insert("options", wire_options(options));
    Ok(body)
}

fn legacy_context(context: &pi_ai::Context) -> Result<OrderedJsonObject, EncodeError> {
    let mut wire = OrderedJsonObject::new();
    let mut messages = OrderedJsonArray::new();
    for message in &context.messages {
        messages.push(legacy_message(message)?);
    }
    wire.insert("messages", messages);
    if let Some(system) = &context.system_prompt {
        wire.insert("systemPrompt", system.clone());
    }
    if !context.tools.is_empty() {
        let mut tools = OrderedJsonArray::new();
        for tool in &context.tools {
            let mut value = OrderedJsonObject::new();
            value.insert("name", tool.name.clone());
            value.insert("description", tool.description.clone());
            value.insert("parameters", tool.parameters.clone());
            if let Some(constrained) = &tool.constrained_sampling {
                value.insert(
                    "constrainedSampling",
                    serde_json::to_value(constrained).map_err(|error| {
                        EncodeError::InvalidRequest {
                            message: format!("could not encode constrained sampling: {error}"),
                        }
                    })?,
                );
            }
            tools.push(value);
        }
        wire.insert("tools", tools);
    }
    Ok(wire)
}

fn legacy_message(message: &Message) -> Result<OrderedJsonObject, EncodeError> {
    match message {
        Message::User(message) => {
            let mut wire = OrderedJsonObject::new();
            wire.insert("role", "user");
            wire.insert("content", legacy_user_content(&message.content));
            wire.insert("timestamp", message.timestamp.0);
            Ok(wire)
        }
        Message::Assistant(message) => {
            let mut wire = OrderedJsonObject::new();
            wire.insert("role", "assistant");
            wire.insert("content", legacy_assistant_content(message));
            wire.insert("api", message.api.as_str());
            wire.insert("provider", message.provider.as_str());
            wire.insert("model", message.requested_model.as_str());
            wire.insert("usage", legacy_usage(&message.usage, message.cost.as_ref()));
            wire.insert("stopReason", legacy_finish_reason(message.finish.reason));
            wire.insert("timestamp", message.timestamp.0);
            // Pi allocates the base assistant object through `createOutput` /
            // `createEventConverter`, then appends response metadata as stream
            // events arrive. Retain that observable property order.
            if let Some(response_model) = &message.response_model {
                wire.insert("responseModel", response_model.as_str());
            }
            if let Some(raw_reason) = &message.finish.raw_provider_reason {
                wire.insert("rawStopReason", raw_reason.clone());
            }
            if let Some(error) = &message.finish.error {
                wire.insert("errorMessage", error.message.clone());
            }
            if let Some(response_id) = &message.response_id {
                wire.insert("responseId", response_id.clone());
            }
            if let Some(end_turn) = message.end_turn {
                wire.insert("endTurn", end_turn);
            }
            if !message.diagnostics.is_empty() {
                wire.insert(
                    "diagnostics",
                    serde_json::to_value(&message.diagnostics).map_err(|error| {
                        EncodeError::InvalidRequest {
                            message: format!("could not encode assistant diagnostics: {error}"),
                        }
                    })?,
                );
            }
            Ok(wire)
        }
        Message::ToolResult(message) => {
            let mut wire = OrderedJsonObject::new();
            wire.insert("role", "toolResult");
            wire.insert("toolCallId", message.tool_call_id.as_str());
            wire.insert("toolName", message.tool_name.clone());
            let mut content = OrderedJsonArray::new();
            for block in &message.content {
                let mut item = OrderedJsonObject::new();
                match block {
                    ToolResultContent::Text { text, .. } => {
                        item.insert("type", "text");
                        item.insert("text", text.clone());
                    }
                    ToolResultContent::Image {
                        data, mime_type, ..
                    } => {
                        item.insert("type", "image");
                        item.insert("mimeType", mime_type.clone());
                        item.insert("data", data.clone());
                    }
                }
                content.push(item);
            }
            wire.insert("content", content);
            if let Some(details) = &message.details {
                wire.insert(
                    "details",
                    serde_json::from_str::<serde_json::Value>(details.value.get()).map_err(
                        |error| EncodeError::InvalidRequest {
                            message: format!("could not encode tool details: {error}"),
                        },
                    )?,
                );
            }
            if let Some(usage) = &message.usage {
                wire.insert("usage", legacy_usage(usage, None));
            }
            if !message.added_tool_names.is_empty() {
                wire.insert(
                    "addedToolNames",
                    OrderedJsonArray::from_iter(
                        message
                            .added_tool_names
                            .iter()
                            .map(|name| OrderedJsonValue::from(name.as_str())),
                    ),
                );
            }
            wire.insert("isError", message.is_error);
            wire.insert("timestamp", message.timestamp.0);
            Ok(wire)
        }
    }
}

fn legacy_user_content(content: &[ContentBlock]) -> OrderedJsonValue {
    if let [ContentBlock::Text { text, .. }] = content {
        return text.clone().into();
    }
    let mut result = OrderedJsonArray::new();
    for block in content {
        let mut wire = OrderedJsonObject::new();
        match block {
            ContentBlock::Text { text, .. } => {
                wire.insert("type", "text");
                wire.insert("text", text.clone());
            }
            ContentBlock::Image {
                data, mime_type, ..
            } => {
                wire.insert("type", "image");
                wire.insert("mimeType", mime_type.clone());
                wire.insert("data", data.clone());
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => continue,
        }
        result.push(wire);
    }
    result.into()
}

fn legacy_assistant_content(message: &pi_ai::AssistantMessage) -> OrderedJsonArray {
    let mut result = OrderedJsonArray::new();
    for (content_index, block) in message.content.iter().enumerate() {
        let mut wire = OrderedJsonObject::new();
        match block {
            ContentBlock::Text { text, .. } => {
                wire.insert("type", "text");
                wire.insert("text", text.clone());
                if let Some(signature) = replay_signature(message, block, content_index) {
                    wire.insert("textSignature", signature);
                }
            }
            ContentBlock::Thinking { text, redacted, .. } => {
                wire.insert("type", "thinking");
                wire.insert("thinking", text.clone());
                if let Some(signature) = replay_signature(message, block, content_index) {
                    wire.insert("thinkingSignature", signature);
                }
                if *redacted
                    || replay_has_kind(message, block, pi_ai::PI_MESSAGES_VISIBLE_THINKING_KIND)
                {
                    wire.insert("redacted", *redacted);
                }
            }
            ContentBlock::ToolCall { call, .. } => {
                wire.insert("type", "toolCall");
                wire.insert("id", call.id.as_str());
                wire.insert("name", call.name.clone());
                wire.insert("arguments", call.arguments.clone());
                if let Some(signature) = replay_signature(message, block, content_index) {
                    wire.insert("thoughtSignature", signature);
                }
            }
            ContentBlock::Image { .. } => continue,
        }
        result.push(wire);
    }
    result
}

fn replay_signature(
    message: &pi_ai::AssistantMessage,
    block: &ContentBlock,
    content_index: usize,
) -> Option<String> {
    let item = message
        .replay
        .items
        .iter()
        .filter(|item| item.completeness == ReplayCompleteness::Complete)
        .filter(|item| match (&item.target, block) {
            (ReplayTarget::ContentBlock(target), _) => target == block.id(),
            (ReplayTarget::ToolCall(target), ContentBlock::ToolCall { call, .. }) => {
                target == &call.id
            }
            (ReplayTarget::ToolCall(_), _) => false,
            (ReplayTarget::ProviderOutputItem { output_index }, _) => {
                usize::try_from(*output_index).ok() == Some(content_index)
            }
            (ReplayTarget::Message, _) => false,
        })
        .filter(|item| signature_kind_matches(block, item.kind.as_str()))
        .min_by_key(|item| item.ordinal)?;
    match &item.payload {
        OpaquePayload::Utf8(value) => Some(value.clone()),
        OpaquePayload::Bytes(value) => Some(BASE64.encode(value)),
        OpaquePayload::JsonBytes(value) => String::from_utf8(value.clone()).ok(),
    }
}

fn signature_kind_matches(block: &ContentBlock, kind: &str) -> bool {
    match block {
        ContentBlock::Text { .. } => matches!(
            kind,
            pi_ai::GOOGLE_THOUGHT_SIGNATURE_KIND
                | pi_ai::OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND
                | pi_ai::PI_MESSAGES_TEXT_SIGNATURE_KIND
        ),
        ContentBlock::Thinking { .. } => {
            kind != pi_ai::OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND
                && kind != pi_ai::OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND
                && kind != pi_ai::PI_MESSAGES_REDACTED_THINKING_KIND
                && kind != pi_ai::PI_MESSAGES_VISIBLE_THINKING_KIND
        }
        ContentBlock::ToolCall { .. } => matches!(
            kind,
            pi_ai::GOOGLE_THOUGHT_SIGNATURE_KIND | pi_ai::OPENAI_CHAT_REASONING_DETAIL_KIND
        ),
        ContentBlock::Image { .. } => false,
    }
}

fn replay_has_kind(message: &pi_ai::AssistantMessage, block: &ContentBlock, kind: &str) -> bool {
    message.replay.items.iter().any(|item| {
        item.completeness == ReplayCompleteness::Complete
            && item.kind.as_str() == kind
            && match (&item.target, block) {
                (ReplayTarget::ContentBlock(target), _) => target == block.id(),
                (ReplayTarget::ToolCall(target), ContentBlock::ToolCall { call, .. }) => {
                    target == &call.id
                }
                _ => false,
            }
    })
}

fn legacy_usage(usage: &pi_ai::Usage, cost: Option<&pi_ai::Cost>) -> OrderedJsonObject {
    let mut wire = OrderedJsonObject::new();
    wire.insert("input", usage.input_tokens);
    wire.insert("output", usage.output_tokens);
    wire.insert("cacheRead", usage.cache_read_tokens.unwrap_or(0));
    wire.insert("cacheWrite", usage.cache_write_tokens.unwrap_or(0));
    if let Some(cache_write_one_hour) = usage.cache_write_one_hour_tokens {
        wire.insert("cacheWrite1h", cache_write_one_hour);
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        wire.insert("reasoning", reasoning);
    }
    wire.insert(
        "totalTokens",
        u64::try_from(usage.total_tokens()).unwrap_or(u64::MAX),
    );
    let mut costs = OrderedJsonObject::new();
    costs.insert("input", 0);
    costs.insert("output", 0);
    costs.insert("cacheRead", 0);
    costs.insert("cacheWrite", 0);
    costs.insert(
        "total",
        cost.map_or(0.into(), |value| money_wire(value.micros)),
    );
    wire.insert("cost", costs);
    wire
}

fn money_wire(micros: i128) -> OrderedJsonValue {
    let magnitude = micros.unsigned_abs();
    let whole = magnitude / 1_000_000;
    let fraction = magnitude % 1_000_000;
    let sign = if micros < 0 { "-" } else { "" };
    let source = if fraction == 0 {
        format!("{sign}{whole}")
    } else {
        let fraction = format!("{fraction:06}").trim_end_matches('0').to_owned();
        format!("{sign}{whole}.{fraction}")
    };
    parse_ordered_json(&source).expect("fixed-point money always renders valid JSON")
}

fn legacy_finish_reason(reason: AssistantFinishReason) -> &'static str {
    match reason {
        AssistantFinishReason::Stop => "stop",
        AssistantFinishReason::Length => "length",
        AssistantFinishReason::ToolUse => "toolUse",
        AssistantFinishReason::Deferred => "deferred",
        AssistantFinishReason::Error => "error",
        AssistantFinishReason::Aborted => "aborted",
    }
}

fn wire_options(options: &PiMessagesOptions) -> OrderedJsonObject {
    let mut wire = OrderedJsonObject::new();
    if let Some(temperature) = options.temperature {
        wire.insert("temperature", temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        wire.insert("maxTokens", max_tokens);
    }
    if let Some(reasoning) = options.reasoning {
        wire.insert(
            "reasoning",
            match reasoning {
                ReasoningLevel::Off => "off",
                ReasoningLevel::Minimal => "minimal",
                ReasoningLevel::Low => "low",
                ReasoningLevel::Medium => "medium",
                ReasoningLevel::High => "high",
                ReasoningLevel::Xhigh => "xhigh",
                ReasoningLevel::Max => "max",
            },
        );
    }
    if let Some(retention) = options.cache_retention {
        wire.insert(
            "cacheRetention",
            match retention {
                CacheRetention::None => "none",
                CacheRetention::Short => "short",
                CacheRetention::Long => "long",
            },
        );
    }
    if let Some(session_id) = &options.session_id {
        wire.insert("sessionId", session_id.clone());
    }
    if let Some(tool_choice) = &options.tool_choice {
        let value: OrderedJsonValue = match tool_choice {
            PiMessagesToolChoice::Auto => "auto".into(),
            PiMessagesToolChoice::None => "none".into(),
            PiMessagesToolChoice::Required => "required".into(),
            PiMessagesToolChoice::Function { name } => {
                let mut function = OrderedJsonObject::new();
                function.insert("name", name.clone());
                let mut choice = OrderedJsonObject::new();
                choice.insert("type", "function");
                choice.insert("function", function);
                choice.into()
            }
        };
        wire.insert("toolChoice", value);
    }
    wire
}
