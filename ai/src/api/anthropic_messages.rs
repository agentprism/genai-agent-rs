//! Anthropic Messages ⇐ pi `src/api/anthropic-messages.ts`.

use crate::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::api::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::api::transform_messages::{normalize_anthropic_tool_call_id, transform_messages};
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::calculate_cost;
use crate::types::{
    AnthropicAllowedFallbackModel, AssistantContent, AssistantMessage, CacheRetention, Context,
    ErrorStopReason, FetchFunction, JsonObject, JsonValue, Message, Model, ProviderBodyStream,
    ProviderEnv, ProviderHeaders, ProviderHttpRequest, ProviderHttpResponse, ProviderResponse,
    SimpleStreamOptions, StopReason, StreamOptions, SuccessfulStopReason, TextContent,
    ThinkingContent, ThinkingLevel, Tool, ToolCall, ToolChoice, ToolResultMessage, UserContent,
    UserContentBlock,
};
use crate::utils::deferred_tools::split_deferred_tools;
use crate::utils::ecma_json::provider_string;
use crate::utils::json_parse::parse_json_with_repair;
use crate::utils::pi_user_agent::get_pi_user_agent;
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{
    ProviderErrorMetadata, ProviderRetryClassify, ProviderRetryError, ProviderRetryOptions,
    retry_provider_request,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use adk_anthropic::Anthropic;
use futures::future::pending;
use futures::{FutureExt, StreamExt};
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CLAUDE_CODE_VERSION: &str = "2.1.75";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const ANTHROPIC_VERSION: &str = "2023-06-01";

const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicToolChoice {
    Mode(AnthropicToolChoiceMode),
    Tool {
        #[serde(rename = "type")]
        kind: AnthropicToolChoiceToolType,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicToolChoiceMode {
    Auto,
    Any,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicToolChoiceToolType {
    Tool,
}

pub trait AnthropicMessagesClient: Send + Sync {
    fn api_key(&self) -> Option<String> {
        None
    }

    fn base_url(&self) -> Option<String> {
        None
    }

    fn messages_url(&self) -> Option<String> {
        None
    }

    fn headers(&self) -> ProviderHeaders {
        ProviderHeaders::new()
    }

    fn fetch(&self) -> Option<Arc<dyn FetchFunction>> {
        None
    }
}

impl AnthropicMessagesClient for Anthropic {
    fn api_key(&self) -> Option<String> {
        Some(self.api_key().to_owned())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<AnthropicEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip)]
    pub client: Option<Arc<dyn AnthropicMessagesClient>>,
}

impl fmt::Debug for AnthropicOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicOptions")
            .field("stream", &self.stream)
            .field("thinking_enabled", &self.thinking_enabled)
            .field("thinking_budget_tokens", &self.thinking_budget_tokens)
            .field("effort", &self.effort)
            .field("thinking_display", &self.thinking_display)
            .field("interleaved_thinking", &self.interleaved_thinking)
            .field("tool_choice", &self.tool_choice)
            .field("client", &self.client.is_some())
            .finish()
    }
}

impl From<StreamOptions> for AnthropicOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessagesApi;

pub fn anthropic_messages_api() -> AnthropicMessagesApi {
    AnthropicMessagesApi
}

impl ProviderStreams for AnthropicMessagesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        match options {
            ApiStreamOptions::Base(options) => stream(model, context, options.into()),
            ApiStreamOptions::AnthropicMessages(options) => stream(model, context, options),
            ApiStreamOptions::BedrockConverseStream(_)
            | ApiStreamOptions::OpenAICompletions(_)
            | ApiStreamOptions::OpenAIResponses(_)
            | ApiStreamOptions::OpenAICodexResponses(_)
            | ApiStreamOptions::GoogleGenerativeAI(_)
            | ApiStreamOptions::GoogleVertex(_)
            | ApiStreamOptions::Custom { .. } => terminal_setup_error(
                model,
                "API options variant does not match anthropic-messages",
            ),
        }
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[derive(Debug, Clone)]
struct ResolvedCompat {
    supports_eager_tool_input_streaming: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    supports_cache_control_on_tools: bool,
    supports_temperature: bool,
    force_adaptive_thinking: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    allowed_fallback_models: Vec<AnthropicAllowedFallbackModel>,
    supports_tool_references: bool,
}

fn compat_value(model: &Model) -> Map<String, Value> {
    model
        .compat
        .as_ref()
        .and_then(|compat| serde_json::to_value(compat).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn compat_bool(compat: &Map<String, Value>, key: &str, fallback: bool) -> bool {
    compat.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

fn get_anthropic_compat(model: &Model) -> ResolvedCompat {
    let value = compat_value(model);
    let allowed_fallback_models = value
        .get("allowedFallbackModels")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    ResolvedCompat {
        supports_eager_tool_input_streaming: compat_bool(
            &value,
            "supportsEagerToolInputStreaming",
            true,
        ),
        supports_long_cache_retention: compat_bool(&value, "supportsLongCacheRetention", true),
        send_session_affinity_headers: compat_bool(&value, "sendSessionAffinityHeaders", false),
        supports_cache_control_on_tools: compat_bool(&value, "supportsCacheControlOnTools", true),
        supports_temperature: compat_bool(&value, "supportsTemperature", true),
        force_adaptive_thinking: compat_bool(&value, "forceAdaptiveThinking", false),
        allow_empty_signature: compat_bool(&value, "allowEmptySignature", false),
        supports_strict_tools: compat_bool(&value, "supportsStrictTools", false),
        allowed_fallback_models,
        supports_tool_references: value
            .get("supportsToolReferences")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| default_supports_tool_references(model)),
    }
}

fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider.as_str() != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let Some(rest) = model.id.strip_prefix("claude-") else {
        return false;
    };
    let Some(rest) = ["opus-", "sonnet-", "fable-"]
        .iter()
        .find_map(|prefix| rest.strip_prefix(prefix))
    else {
        return false;
    };
    let mut components = rest.split('-');
    let Some(major_text) = components.next().filter(|value| !value.is_empty()) else {
        return false;
    };
    let Ok(major) = major_text.parse::<u64>() else {
        return false;
    };
    let minor = components
        .next()
        .filter(|value| value.len() < 8)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    major > 4 || (major == 4 && minor >= 5)
}

fn resolve_cache_retention(
    retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    retention.unwrap_or_else(|| {
        if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn cache_control(model: &Model, options: &AnthropicOptions) -> Option<Value> {
    let retention = resolve_cache_retention(
        options.stream.cache_retention,
        options.stream.request.env.as_ref(),
    );
    if retention == CacheRetention::None {
        return None;
    }
    let compat = get_anthropic_compat(model);
    if retention == CacheRetention::Long && compat.supports_long_cache_retention {
        Some(json!({"type":"ephemeral", "ttl":"1h"}))
    } else {
        Some(json!({"type":"ephemeral"}))
    }
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |candidate| (*candidate).to_owned())
}

fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    tools
        .iter()
        .flat_map(|tools| tools.iter())
        .find(|tool| tool.name.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |tool| tool.name.clone())
}

fn convert_content_blocks(content: &[UserContentBlock]) -> JsonValue {
    let has_images = content
        .iter()
        .any(|block| matches!(block, UserContentBlock::Image(_)));
    if !has_images {
        return JsonValue::String(
            sanitize_surrogates(&crate::types::JsString::join_refs(
                content.iter().filter_map(|block| match block {
                    UserContentBlock::Text(text) => Some(&text.text),
                    UserContentBlock::Image(_) => None,
                }),
                "\n",
            ))
            .into(),
        );
    }
    let mut blocks = content
        .iter()
        .map(|block| match block {
            UserContentBlock::Text(text) => {
                JsonValue::from(json!({"type":"text", "text":sanitize_surrogates(&text.text)}))
            }
            UserContentBlock::Image(image) => {
                let mut source = JsonObject::new();
                source.insert("type", "base64");
                source.insert("media_type", image.mime_type.clone());
                source.insert("data", image.data.clone());
                let mut block = JsonObject::new();
                block.insert("type", "image");
                block.insert("source", JsonValue::Object(source));
                JsonValue::Object(block)
            }
        })
        .collect::<Vec<_>>();
    if !blocks.iter().any(|block| {
        block
            .get("type")
            .and_then(JsonValue::as_str)
            .is_some_and(|kind| kind == "text")
    }) {
        blocks.insert(
            0,
            JsonValue::from(json!({"type":"text", "text":"(see attached image)"})),
        );
    }
    JsonValue::Array(blocks)
}

fn add_ecma_cache_control(block: &mut JsonValue, cache_control: &Value) {
    if let Some(block) = block.as_object_mut() {
        block.insert("cache_control", JsonValue::from(cache_control.clone()));
    }
}

fn convert_tool_result(
    message: &ToolResultMessage,
    is_oauth: bool,
    deferred_tool_names: &IndexSet<String>,
    loaded_tool_names: &mut IndexSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (JsonValue, Vec<JsonValue>) {
    let mut references = Vec::new();
    for name in message.added_tool_names.iter().flatten() {
        let normalized = normalize_tool_name(name);
        if !deferred_tool_names.contains(&normalized) || loaded_tool_names.contains(&normalized) {
            continue;
        }
        loaded_tool_names.insert(normalized);
        let mut reference = JsonObject::new();
        reference.insert("type", "tool_reference");
        reference.insert(
            "tool_name",
            if is_oauth {
                JsonValue::from(to_claude_code_name(name))
            } else {
                provider_string(name)
            },
        );
        references.push(JsonValue::Object(reference));
    }
    let converted_content = convert_content_blocks(&message.content);
    let has_references = !references.is_empty();
    let content = if references.is_empty() {
        converted_content.clone()
    } else {
        JsonValue::Array(references)
    };
    let mut tool_result = JsonObject::new();
    tool_result.insert("type", "tool_result");
    tool_result.insert("tool_use_id", provider_string(&message.tool_call_id));
    tool_result.insert("content", content);
    tool_result.insert("is_error", message.is_error);
    let sibling_content = if !has_references {
        Vec::new()
    } else {
        match converted_content {
            JsonValue::String(text) => {
                let mut block = JsonObject::new();
                block.insert("type", "text");
                block.insert("text", text);
                vec![JsonValue::Object(block)]
            }
            JsonValue::Array(blocks) => blocks,
            _ => Vec::new(),
        }
    };
    (JsonValue::Object(tool_result), sibling_content)
}

fn convert_messages(
    messages: &[Message],
    is_oauth: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    deferred_tool_names: &IndexSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> Vec<JsonValue> {
    let mut params = Vec::new();
    let mut loaded_tool_names = IndexSet::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(message) => match &message.content {
                UserContent::Text(text) if !text.is_blank() => {
                    params.push(JsonValue::from(json!({
                        "role":"user",
                        "content":sanitize_surrogates(text),
                    })))
                }
                UserContent::Text(_) => {}
                UserContent::Blocks(content) => {
                    let blocks = content
                        .iter()
                        .filter_map(|block| match block {
                            UserContentBlock::Text(text) if text.text.is_blank() => None,
                            UserContentBlock::Text(text) => Some(JsonValue::from(json!({
                                "type":"text",
                                "text":sanitize_surrogates(&text.text),
                            }))),
                            UserContentBlock::Image(image) => {
                                let mut source = JsonObject::new();
                                source.insert("type", "base64");
                                source.insert("media_type", image.mime_type.clone());
                                source.insert("data", image.data.clone());
                                let mut image_block = JsonObject::new();
                                image_block.insert("type", "image");
                                image_block.insert("source", JsonValue::Object(source));
                                Some(JsonValue::Object(image_block))
                            }
                        })
                        .collect::<Vec<_>>();
                    if !blocks.is_empty() {
                        let mut message = JsonObject::new();
                        message.insert("role", "user");
                        message.insert("content", JsonValue::Array(blocks));
                        params.push(JsonValue::Object(message));
                    }
                }
            },
            Message::Assistant(message) => {
                let mut blocks = Vec::new();
                for block in &message.content {
                    match block {
                        AssistantContent::Text(text) if !text.text.is_blank() => {
                            blocks.push(JsonValue::from(
                                json!({"type":"text", "text":sanitize_surrogates(&text.text)}),
                            ))
                        }
                        AssistantContent::Text(_) => {}
                        AssistantContent::Thinking(thinking) if thinking.redacted == Some(true) => {
                            let mut redacted = JsonObject::new();
                            redacted.insert("type", "redacted_thinking");
                            if let Some(signature) = &thinking.thinking_signature {
                                redacted.insert("data", provider_string(signature));
                            }
                            blocks.push(JsonValue::Object(redacted));
                        }
                        AssistantContent::Thinking(thinking) => {
                            let signature = thinking.thinking_signature.as_ref();
                            let has_signature = signature.is_some_and(|value| !value.is_blank());
                            if thinking.thinking.is_blank() && !has_signature {
                                continue;
                            }
                            if has_signature || allow_empty_signature {
                                let mut block = JsonObject::new();
                                block.insert("type", "thinking");
                                block.insert("thinking", sanitize_surrogates(&thinking.thinking));
                                block.insert("signature", signature.cloned().unwrap_or_default());
                                blocks.push(JsonValue::Object(block));
                            } else {
                                blocks.push(JsonValue::from(json!({
                                    "type":"text",
                                    "text":sanitize_surrogates(&thinking.thinking),
                                })));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let mut block = JsonObject::new();
                            block.insert("type", "tool_use");
                            block.insert("id", provider_string(&call.id));
                            block.insert(
                                "name",
                                if is_oauth {
                                    JsonValue::from(to_claude_code_name(&call.name))
                                } else {
                                    provider_string(&call.name)
                                },
                            );
                            block.insert("input", call.arguments.to_provider_json());
                            blocks.push(JsonValue::Object(block));
                        }
                    }
                }
                if !blocks.is_empty() {
                    let mut message = JsonObject::new();
                    message.insert("role", "assistant");
                    message.insert("content", JsonValue::Array(blocks));
                    params.push(JsonValue::Object(message));
                }
            }
            Message::ToolResult(_) => {
                let mut tool_results = Vec::new();
                let mut sibling_content = Vec::new();
                let mut next = index;
                while let Some(Message::ToolResult(message)) = messages.get(next) {
                    let (tool_result, siblings) = convert_tool_result(
                        message,
                        is_oauth,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_tool_name,
                    );
                    tool_results.push(tool_result);
                    sibling_content.extend(siblings);
                    next += 1;
                }
                tool_results.extend(sibling_content);
                let mut message = JsonObject::new();
                message.insert("role", "user");
                message.insert("content", JsonValue::Array(tool_results));
                params.push(JsonValue::Object(message));
                index = next - 1;
            }
        }
        index += 1;
    }

    if let Some(cache_control) = cache_control
        && let Some(last) = params.last_mut()
        && last
            .get("role")
            .and_then(JsonValue::as_str)
            .is_some_and(|role| role == "user")
        && let Some(content) = last
            .as_object_mut()
            .and_then(|message| message.get_mut("content"))
    {
        match content {
            JsonValue::Array(blocks) => {
                if let Some(block) = blocks.last_mut()
                    && matches!(
                        block.get("type").and_then(JsonValue::as_str),
                        Some(kind) if matches!(kind.as_str(), "text" | "image" | "tool_result")
                    )
                {
                    add_ecma_cache_control(block, cache_control);
                }
            }
            JsonValue::String(text) => {
                let text = text.clone();
                let mut block = JsonObject::new();
                block.insert("type", "text");
                block.insert("text", text);
                block.insert("cache_control", JsonValue::from(cache_control.clone()));
                *content = JsonValue::Array(vec![JsonValue::Object(block)]);
            }
            _ => {}
        }
    }
    params
}

fn convert_tools(
    tools: &[Tool],
    is_oauth: bool,
    compat: &ResolvedCompat,
    cache_control: Option<&Value>,
    defer_loading: bool,
) -> Result<Vec<Value>, AnthropicError> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_tools)
                .map_err(AnthropicError::display)?;
            let parameters =
                get_json_schema_tool_parameters(tool, strict).map_err(AnthropicError::display)?;
            let mut legacy = Map::from_iter([
                ("type".to_owned(), Value::String("object".to_owned())),
                (
                    "properties".to_owned(),
                    parameters
                        .get("properties")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new())),
                ),
                (
                    "required".to_owned(),
                    parameters
                        .get("required")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(Vec::new())),
                ),
            ]);
            if strict == Some(true)
                && let Some(parameters) = parameters.as_object()
            {
                let mut merged = parameters.clone();
                merged.extend(legacy);
                legacy = merged;
            }
            let mut result = Map::from_iter([
                (
                    "name".to_owned(),
                    Value::String(if is_oauth {
                        to_claude_code_name(&tool.name)
                    } else {
                        tool.name.clone()
                    }),
                ),
                (
                    "description".to_owned(),
                    Value::String(tool.description.clone()),
                ),
                ("input_schema".to_owned(), Value::Object(legacy)),
            ]);
            if compat.supports_eager_tool_input_streaming {
                result.insert("eager_input_streaming".to_owned(), Value::Bool(true));
            }
            if strict == Some(true) {
                result.insert("strict".to_owned(), Value::Bool(true));
            }
            if defer_loading {
                result.insert("defer_loading".to_owned(), Value::Bool(true));
            }
            if index + 1 == tools.len()
                && let Some(cache_control) = cache_control
            {
                result.insert("cache_control".to_owned(), cache_control.clone());
            }
            Ok(Value::Object(result))
        })
        .collect()
}

fn build_params(
    model: &Model,
    context: &Context,
    is_oauth: bool,
    options: &AnthropicOptions,
) -> Result<JsonValue, AnthropicError> {
    let cache_control = cache_control(model, options);
    let compat = get_anthropic_compat(model);
    let transformed = transform_messages(
        &context.messages,
        model,
        Some(&|id, _, _| normalize_anthropic_tool_call_id(&id.to_utf8_lossy()).into()),
    );
    let normalize_tool_name = |name: &str| {
        if is_oauth {
            to_claude_code_name(name)
        } else {
            name.to_owned()
        }
    };
    let placement = split_deferred_tools(
        &Context {
            system_prompt: context.system_prompt.clone(),
            messages: transformed.clone(),
            tools: context.tools.clone(),
        },
        compat.supports_tool_references,
        normalize_tool_name,
    );
    let mut immediate = placement.immediate;
    let mut deferred = placement.deferred.into_values().collect::<Vec<_>>();
    if immediate.is_empty() && !deferred.is_empty() {
        immediate = deferred;
        deferred = Vec::new();
    }
    let deferred_names = deferred
        .iter()
        .map(|tool| normalize_tool_name(&tool.name))
        .collect::<IndexSet<_>>();
    let messages = convert_messages(
        &transformed,
        is_oauth,
        cache_control.as_ref(),
        compat.allow_empty_signature,
        &deferred_names,
        &normalize_tool_name,
    );
    let mut params = JsonObject::new();
    params.insert("model", model.id.clone());
    params.insert("messages", JsonValue::Array(messages));
    params.insert(
        "max_tokens",
        options.stream.max_tokens.unwrap_or(model.max_tokens),
    );
    params.insert("stream", true);

    if is_oauth {
        let mut system = vec![system_block(CLAUDE_CODE_IDENTITY, cache_control.as_ref())];
        if let Some(prompt) = context
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
        {
            system.push(system_block(prompt, cache_control.as_ref()));
        }
        params.insert(
            "system",
            JsonValue::Array(system.into_iter().map(JsonValue::from).collect()),
        );
    } else if let Some(prompt) = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
    {
        params.insert(
            "system",
            JsonValue::Array(vec![JsonValue::from(system_block(
                prompt,
                cache_control.as_ref(),
            ))]),
        );
    }

    if let Some(temperature) = options.stream.temperature
        && options.thinking_enabled != Some(true)
        && compat.supports_temperature
    {
        params.insert("temperature", temperature);
    }

    if !immediate.is_empty() || !deferred.is_empty() {
        let immediate_cache = compat
            .supports_cache_control_on_tools
            .then_some(cache_control.as_ref())
            .flatten();
        let mut tools = convert_tools(&immediate, is_oauth, &compat, immediate_cache, false)?;
        tools.extend(convert_tools(&deferred, is_oauth, &compat, None, true)?);
        params.insert(
            "tools",
            JsonValue::Array(tools.into_iter().map(JsonValue::from).collect()),
        );
    }

    if model.reasoning {
        if options.thinking_enabled == Some(true) {
            let display = options
                .thinking_display
                .unwrap_or(AnthropicThinkingDisplay::Summarized);
            if compat.force_adaptive_thinking {
                params.insert(
                    "thinking",
                    JsonValue::from(json!({"type":"adaptive", "display":display})),
                );
                if let Some(effort) = options.effort {
                    params.insert("output_config", JsonValue::from(json!({"effort":effort})));
                }
            } else {
                params.insert(
                    "thinking",
                    JsonValue::from(json!({
                        "type":"enabled",
                        "budget_tokens":crate::types::js_f64_value(
                            options.thinking_budget_tokens
                                .filter(|budget| *budget != 0.0)
                                .unwrap_or(1_024.0)
                        ),
                        "display":display,
                    })),
                );
            }
        } else if options.thinking_enabled == Some(false)
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.off.as_ref())
                .is_none_or(Option::is_some)
        {
            params.insert("thinking", JsonValue::from(json!({"type":"disabled"})));
        }
    }

    if let Some(user_id) = options
        .stream
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
    {
        params.insert("metadata", JsonValue::from(json!({"user_id":user_id})));
    }
    if let Some(tool_choice) = &options.tool_choice {
        let value = match tool_choice {
            AnthropicToolChoice::Mode(mode) => json!({"type":mode}),
            AnthropicToolChoice::Tool { .. } => {
                serde_json::to_value(tool_choice).map_err(AnthropicError::display)?
            }
        };
        params.insert("tool_choice", JsonValue::from(value));
    }
    if !compat.allowed_fallback_models.is_empty() {
        params.insert(
            "fallbacks",
            JsonValue::Array(
                compat
                    .allowed_fallback_models
                    .iter()
                    .map(|fallback| JsonValue::from(json!({"model":fallback.model})))
                    .collect(),
            ),
        );
    }
    Ok(JsonValue::Object(params))
}

fn system_block(text: &str, cache_control: Option<&Value>) -> Value {
    let mut block = json!({"type":"text", "text":sanitize_surrogates(text)});
    if let Some(cache_control) = cache_control
        && let Some(block) = block.as_object_mut()
    {
        block.insert("cache_control".to_owned(), cache_control.clone());
    }
    block
}

pub fn stream(
    model: &Model,
    context: &Context,
    options: AnthropicOptions,
) -> AssistantMessageEventStream {
    let model = model.clone();
    let context = context.clone();
    let (sender, stream) = AssistantMessageEventStream::channel();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return terminal_setup_error(&model, "Tokio runtime is not available");
    };
    runtime.spawn(async move {
        run_stream(sender, model, context, options).await;
    });
    stream
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    if let Err(error) = assert_request_auth(
        model.provider.as_str(),
        options.stream.request.api_key.as_deref(),
        options.stream.request.headers.as_ref(),
    ) {
        return terminal_setup_error(model, &error.message);
    }
    let mut base = AnthropicOptions {
        stream: build_base_options(
            model,
            context,
            Some(&options),
            options.stream.request.api_key.as_deref(),
        ),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => AnthropicToolChoice::Mode(AnthropicToolChoiceMode::Auto),
            ToolChoice::None => AnthropicToolChoice::Mode(AnthropicToolChoiceMode::None),
        }),
        ..AnthropicOptions::default()
    };
    let Some(reasoning) = options.reasoning else {
        base.thinking_enabled = Some(false);
        return stream(model, context, base);
    };
    let compat = get_anthropic_compat(model);
    if compat.force_adaptive_thinking {
        base.thinking_enabled = Some(true);
        base.effort = Some(map_thinking_level_to_effort(model, reasoning));
        return stream(model, context, base);
    }
    let adjusted = adjust_max_tokens_for_thinking(
        base.stream.max_tokens,
        model.max_tokens,
        reasoning,
        options.thinking_budgets.as_ref(),
    );
    let max_tokens = clamp_max_tokens_to_context(model, context, adjusted.max_tokens);
    base.stream.max_tokens = Some(max_tokens);
    base.thinking_enabled = Some(true);
    base.thinking_budget_tokens = Some(
        adjusted
            .thinking_budget
            .min((max_tokens - 1_024.0).max(0.0)),
    );
    stream(model, context, base)
}

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> AnthropicEffort {
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| match level {
            ThinkingLevel::Minimal => map.minimal.as_ref(),
            ThinkingLevel::Low => map.low.as_ref(),
            ThinkingLevel::Medium => map.medium.as_ref(),
            ThinkingLevel::High => map.high.as_ref(),
            ThinkingLevel::Xhigh => map.xhigh.as_ref(),
            ThinkingLevel::Max => map.max.as_ref(),
        });
    if let Some(Some(mapped)) = mapped {
        return match mapped.as_str() {
            "low" => AnthropicEffort::Low,
            "medium" => AnthropicEffort::Medium,
            "high" => AnthropicEffort::High,
            "xhigh" => AnthropicEffort::Xhigh,
            "max" => AnthropicEffort::Max,
            _ => fallback_effort(level),
        };
    }
    fallback_effort(level)
}

fn fallback_effort(level: ThinkingLevel) -> AnthropicEffort {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => AnthropicEffort::Low,
        ThinkingLevel::Medium => AnthropicEffort::Medium,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => AnthropicEffort::High,
    }
}

fn terminal_setup_error(model: &Model, message: &str) -> AssistantMessageEventStream {
    let mut output = pending_message(model);
    output.stop_reason = StopReason::Error;
    output.error_message = Some(message.into());
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error: output,
    }])
}

fn pending_message(model: &Model) -> AssistantMessage {
    AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    )
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

async fn run_stream(
    sender: AssistantStreamSender,
    model: Model,
    context: Context,
    options: AnthropicOptions,
) {
    let mut output = pending_message(&model);
    let result = AssertUnwindSafe(run_stream_inner(
        &sender,
        &model,
        &context,
        &options,
        &mut output,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| {
        Err(AnthropicError::new(
            crate::utils::diagnostics::format_panic_payload(panic.as_ref()),
        ))
    });
    if let Err(error) = result {
        output.stop_reason = if options
            .stream
            .request
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
            || error.aborted
        {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(error.message.into());
        let reason = if output.stop_reason == StopReason::Aborted {
            ErrorStopReason::Aborted
        } else {
            ErrorStopReason::Error
        };
        let _ = sender.send(AssistantMessageEvent::Error {
            reason,
            error: output,
        });
    }
}

async fn run_stream_inner(
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &AnthropicOptions,
    output: &mut AssistantMessage,
) -> Result<(), AnthropicError> {
    let (api_key, base_url, messages_url, is_oauth) = if let Some(client) = &options.client {
        (
            client.api_key(),
            client.base_url().unwrap_or_else(|| model.base_url.clone()),
            client.messages_url(),
            false,
        )
    } else {
        assert_request_auth(
            model.provider.as_str(),
            options.stream.request.api_key.as_deref(),
            options.stream.request.headers.as_ref(),
        )?;
        let api_key = options.stream.request.api_key.clone();
        let is_oauth = api_key.as_deref().is_some_and(is_oauth_token);
        (api_key, model.base_url.clone(), None, is_oauth)
    };
    let compat = get_anthropic_compat(model);
    let mut params = build_params(model, context, is_oauth, options)?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(params.clone(), model)
            .await
            .map_err(AnthropicError::new)?
    {
        params = replacement;
    }
    let (headers, fetch) = if let Some(client) = &options.client {
        (
            create_prebuilt_client_headers(client.as_ref()),
            client.fetch(),
        )
    } else {
        (
            create_headers(
                model,
                context,
                api_key.as_deref(),
                is_oauth,
                options,
                &compat,
            ),
            options.stream.request.fetch.clone(),
        )
    };
    let request = AnthropicSseRequest {
        url: messages_url
            .unwrap_or_else(|| format!("{}/v1/messages", base_url.trim_end_matches('/'))),
        headers,
        body: crate::utils::ecma_json::stringify_provider_json(&params).into_bytes(),
        fetch,
        signal: options.stream.request.signal.clone(),
        timeout_ms: options.stream.request.timeout_ms,
    };
    let retry_options = ProviderRetryOptions {
        max_retries: options.stream.request.max_retries,
        max_retry_delay_ms: options.stream.request.max_retry_delay_ms,
        signal: options.stream.request.signal.clone(),
    };
    let acquired = retry_provider_request(|| acquire_sse(&request), retry_options)
        .await
        .map_err(format_retry_error)?;
    if let Some(on_response) = &options.stream.request.on_response {
        on_response(acquired.response.clone(), model)
            .await
            .map_err(AnthropicError::new)?;
    }
    sender
        .send(AssistantMessageEvent::Start {
            partial: Arc::new(output.clone()),
        })
        .map_err(AnthropicError::display)?;
    process_sse_body(
        acquired.body,
        options.stream.request.signal.clone(),
        sender,
        model,
        context,
        is_oauth,
        &compat,
        output,
    )
    .await?;
    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(AnthropicError::aborted("Request was aborted"));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(AnthropicError::new(
            "Anthropic stream ended without a stop reason",
        ));
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        return Err(AnthropicError::new(
            output.error_message.as_ref().map_or_else(
                || "An unknown error occurred".to_owned(),
                |message| message.to_utf8_lossy(),
            ),
        ));
    }
    let reason = successful_stop_reason(output.stop_reason)
        .ok_or_else(|| AnthropicError::new("An unknown error occurred"))?;
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(AnthropicError::display)
}

fn successful_stop_reason(reason: StopReason) -> Option<SuccessfulStopReason> {
    match reason {
        StopReason::Stop => Some(SuccessfulStopReason::Stop),
        StopReason::Length => Some(SuccessfulStopReason::Length),
        StopReason::ToolUse => Some(SuccessfulStopReason::ToolUse),
        StopReason::Deferred => Some(SuccessfulStopReason::Deferred),
        StopReason::Pending | StopReason::Error | StopReason::Aborted => None,
    }
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case(name)
                && value.as_ref().is_some_and(|value| !value.trim().is_empty())
        })
    })
}

fn assert_request_auth(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<(), AnthropicError> {
    if api_key.is_some_and(|api_key| !api_key.is_empty())
        || has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
    {
        return Ok(());
    }
    Err(AnthropicError::new(format!(
        "No API key for provider: {provider}"
    )))
}

fn create_headers(
    model: &Model,
    context: &Context,
    api_key: Option<&str>,
    is_oauth: bool,
    options: &AnthropicOptions,
    compat: &ResolvedCompat,
) -> BTreeMap<String, String> {
    let use_fine_grained = context
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        && !compat.supports_eager_tool_input_streaming;
    let use_interleaved =
        options.interleaved_thinking.unwrap_or(true) && !compat.force_adaptive_thinking;
    let mut betas = Vec::new();
    if use_fine_grained {
        betas.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if use_interleaved {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    if !compat.allowed_fallback_models.is_empty() {
        betas.push(SERVER_SIDE_FALLBACK_BETA);
    }
    let mut headers = BTreeMap::from([
        ("accept".to_owned(), "application/json".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
        (
            "anthropic-dangerous-direct-browser-access".to_owned(),
            "true".to_owned(),
        ),
        ("User-Agent".to_owned(), get_pi_user_agent()),
    ]);
    if model.provider.as_str() == "github-copilot" {
        if !betas.is_empty() {
            headers.insert("anthropic-beta".to_owned(), betas.join(","));
        }
        headers.extend(build_copilot_dynamic_headers(
            &context.messages,
            has_copilot_vision_input(&context.messages),
        ));
        if let Some(api_key) = api_key {
            headers.insert("Authorization".to_owned(), format!("Bearer {api_key}"));
        }
    } else if is_oauth {
        let mut oauth_betas = vec!["claude-code-20250219", "oauth-2025-04-20"];
        oauth_betas.extend(betas);
        headers.insert("anthropic-beta".to_owned(), oauth_betas.join(","));
        replace_header(
            &mut headers,
            "user-agent",
            Some(&format!("claude-cli/{CLAUDE_CODE_VERSION}")),
        );
        headers.insert("x-app".to_owned(), "cli".to_owned());
        if let Some(api_key) = api_key {
            headers.insert("Authorization".to_owned(), format!("Bearer {api_key}"));
        }
    } else {
        if !betas.is_empty() {
            headers.insert("anthropic-beta".to_owned(), betas.join(","));
        }
        let cache_retention = resolve_cache_retention(
            options.stream.cache_retention,
            options.stream.request.env.as_ref(),
        );
        if cache_retention != CacheRetention::None
            && compat.send_session_affinity_headers
            && let Some(session_id) = options
                .stream
                .session_id
                .as_deref()
                .filter(|session_id| !session_id.is_empty())
        {
            headers.insert("x-session-affinity".to_owned(), session_id.to_owned());
        }
        if let Some(api_key) = api_key {
            headers.insert("x-api-key".to_owned(), api_key.to_owned());
        }
    }
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            replace_header(&mut headers, name, Some(value));
        }
    }
    if let Some(option_headers) = &options.stream.request.headers {
        for (name, value) in option_headers {
            replace_header(&mut headers, name, value.as_deref());
        }
    }
    headers
}

fn create_prebuilt_client_headers(
    client: &dyn AnthropicMessagesClient,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        ("accept".to_owned(), "text/event-stream".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
    ]);
    if let Some(api_key) = client.api_key() {
        headers.insert("x-api-key".to_owned(), api_key);
    }
    for (name, value) in client.headers() {
        replace_header(&mut headers, &name, value.as_deref());
    }
    headers
}

fn replace_header(headers: &mut BTreeMap<String, String>, name: &str, value: Option<&str>) {
    headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
    if let Some(value) = value {
        headers.insert(name.to_owned(), value.to_owned());
    }
}

#[derive(Debug)]
struct AnthropicError {
    message: String,
    aborted: bool,
}

impl AnthropicError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: true,
        }
    }

    fn display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for AnthropicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug)]
struct AnthropicHttpError {
    error: AnthropicError,
    metadata: ProviderErrorMetadata,
}

impl AnthropicHttpError {
    fn transport(message: impl Into<String>, aborted: bool) -> Self {
        Self {
            error: if aborted {
                AnthropicError::aborted(message)
            } else {
                AnthropicError::new(message)
            },
            metadata: ProviderErrorMetadata::default(),
        }
    }

    fn http(status: u16, headers: BTreeMap<String, String>, body: &[u8]) -> Self {
        let text = String::from_utf8_lossy(body).into_owned();
        let message = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|message| !message.is_empty())
            .unwrap_or(text);
        Self {
            error: AnthropicError::new(message),
            metadata: ProviderErrorMetadata {
                status: Some(status),
                headers,
            },
        }
    }
}

impl fmt::Display for AnthropicHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl ProviderRetryClassify for AnthropicHttpError {
    fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata> {
        Some(&self.metadata)
    }

    fn provider_error_message(&self) -> String {
        self.error.message.clone()
    }
}

fn format_retry_error(error: ProviderRetryError<AnthropicHttpError>) -> AnthropicError {
    match error {
        ProviderRetryError::Original(error) => error.error,
        ProviderRetryError::Abort => AnthropicError::aborted("Request aborted"),
        error @ ProviderRetryError::ServerDelay { .. } => AnthropicError::display(error),
    }
}

#[derive(Clone)]
struct AnthropicSseRequest {
    url: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    fetch: Option<Arc<dyn FetchFunction>>,
    signal: Option<Arc<dyn crate::types::AbortSignal>>,
    timeout_ms: Option<f64>,
}

struct AcquiredSse {
    response: ProviderResponse,
    body: Option<ProviderBodyStream>,
}

async fn acquire_sse(request: &AnthropicSseRequest) -> Result<AcquiredSse, AnthropicHttpError> {
    let response = send_request(request).await?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let headers = response.headers.clone();
        let body = read_body(response.body, request.signal.clone()).await?;
        return Err(AnthropicHttpError::http(status, headers, &body));
    }
    let metadata = ProviderResponse {
        status: f64::from(response.status),
        headers: response.headers,
    };
    Ok(AcquiredSse {
        response: metadata,
        body: response.body,
    })
}

async fn send_request(
    request: &AnthropicSseRequest,
) -> Result<ProviderHttpResponse, AnthropicHttpError> {
    if request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(AnthropicHttpError::transport("Request was aborted", true));
    }
    let send = async {
        if let Some(fetch) = &request.fetch {
            return fetch
                .fetch(ProviderHttpRequest {
                    method: "POST".to_owned(),
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    body: Some(request.body.clone()),
                    signal: request.signal.clone(),
                })
                .await
                .map_err(|error| AnthropicHttpError::transport(error, false));
        }
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let mut headers = http::HeaderMap::new();
        for (name, value) in &request.headers {
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| AnthropicHttpError::transport(error.to_string(), false))?,
                http::HeaderValue::from_str(value)
                    .map_err(|error| AnthropicHttpError::transport(error.to_string(), false))?,
            );
        }
        let response = CLIENT
            .get_or_init(reqwest::Client::new)
            .post(&request.url)
            .headers(headers)
            .body(request.body.clone())
            .send()
            .await
            .map_err(|error| AnthropicHttpError::transport(error.to_string(), false))?;
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let body = response
            .bytes_stream()
            .map(|chunk| {
                chunk
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| error.to_string())
            })
            .boxed();
        Ok(ProviderHttpResponse {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or_default().to_owned(),
            headers,
            body: Some(body),
        })
    };
    tokio::pin!(send);
    let timeout_ms = request.timeout_ms.unwrap_or(600_000.0);
    let timeout = if timeout_ms.is_finite() && timeout_ms > 0.0 {
        Duration::from_secs_f64(timeout_ms / 1_000.0)
    } else {
        Duration::ZERO
    };
    tokio::select! {
        () = wait_for_abort(request.signal.clone()) => {
            Err(AnthropicHttpError::transport("Request was aborted", true))
        }
        () = tokio::time::sleep(timeout) => {
            Err(AnthropicHttpError::transport("Request timed out.", false))
        }
        response = &mut send => response,
    }
}

async fn read_body(
    mut body: Option<ProviderBodyStream>,
    signal: Option<Arc<dyn crate::types::AbortSignal>>,
) -> Result<Vec<u8>, AnthropicHttpError> {
    let Some(body) = body.as_mut() else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            () = wait_for_abort(signal.clone()) => {
                return Err(AnthropicHttpError::transport("Request was aborted", true));
            }
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(chunk)) => bytes.extend(chunk),
            Some(Err(error)) => return Err(AnthropicHttpError::transport(error, false)),
            None => return Ok(bytes),
        }
    }
}

async fn wait_for_abort(signal: Option<Arc<dyn crate::types::AbortSignal>>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => pending::<()>().await,
    }
}

#[derive(Debug, Clone)]
struct ServerSentEvent {
    event: Option<String>,
    data: String,
    raw: Vec<String>,
}

#[derive(Debug, Default)]
struct SseDecoder {
    bytes: Vec<u8>,
    text: String,
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<ServerSentEvent> {
        self.bytes.extend_from_slice(bytes);
        self.decode_utf8(eof);
        let mut events = Vec::new();
        while let Some((line, consumed)) = next_sse_line(&self.text) {
            self.text.drain(..consumed);
            if let Some(event) = self.decode_line(&line) {
                events.push(event);
            }
        }
        if eof {
            if !self.text.is_empty() {
                let line = std::mem::take(&mut self.text);
                if let Some(event) = self.decode_line(&line) {
                    events.push(event);
                }
            }
            if let Some(event) = self.flush() {
                events.push(event);
            }
        }
        events
    }

    fn decode_utf8(&mut self, eof: bool) {
        loop {
            match std::str::from_utf8(&self.bytes) {
                Ok(text) => {
                    self.text.push_str(text);
                    self.bytes.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let prefix = String::from_utf8_lossy(&self.bytes[..valid]);
                        self.text.push_str(&prefix);
                        self.bytes.drain(..valid);
                    }
                    match error.error_len() {
                        Some(length) => {
                            self.text.push('\u{fffd}');
                            self.bytes.drain(..length.min(self.bytes.len()));
                        }
                        None if eof => {
                            self.text.push('\u{fffd}');
                            self.bytes.clear();
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        self.raw.push(line.to_owned());
        if line.starts_with(':') {
            return None;
        }
        let (field, mut value) = line
            .split_once(':')
            .map_or((line, ""), |(field, value)| (field, value));
        if let Some(stripped) = value.strip_prefix(' ') {
            value = stripped;
        }
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(ServerSentEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
            raw: std::mem::take(&mut self.raw),
        })
    }
}

fn next_sse_line(text: &str) -> Option<(String, usize)> {
    let (index, delimiter) = text
        .char_indices()
        .find(|(_, character)| matches!(character, '\r' | '\n'))?;
    let mut consumed = index + delimiter.len_utf8();
    if delimiter == '\r' && text.as_bytes().get(consumed) == Some(&b'\n') {
        consumed += 1;
    }
    Some((text[..index].to_owned(), consumed))
}

#[derive(Debug)]
enum ActiveBlock {
    Text {
        content_index: usize,
    },
    Thinking {
        content_index: usize,
    },
    ToolCall {
        content_index: usize,
        partial_json: String,
    },
}

#[allow(clippy::too_many_arguments)]
async fn process_sse_body(
    body: Option<ProviderBodyStream>,
    signal: Option<Arc<dyn crate::types::AbortSignal>>,
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    is_oauth: bool,
    compat: &ResolvedCompat,
    output: &mut AssistantMessage,
) -> Result<(), AnthropicError> {
    let Some(mut body) = body else {
        return Err(AnthropicError::new(
            "Attempted to iterate over an Anthropic response with no body",
        ));
    };
    let mut decoder = SseDecoder::default();
    let mut blocks = BTreeMap::<usize, ActiveBlock>::new();
    let mut usage_model = model.clone();
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    loop {
        let chunk = tokio::select! {
            () = wait_for_abort(signal.clone()) => {
                return Err(AnthropicError::aborted("Request was aborted"));
            }
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(chunk)) => {
                for event in decoder.push(&chunk, false) {
                    process_sse_event(
                        event,
                        sender,
                        model,
                        context,
                        is_oauth,
                        compat,
                        output,
                        &mut usage_model,
                        &mut blocks,
                        &mut saw_message_start,
                        &mut saw_message_stop,
                    )?;
                }
            }
            Some(Err(error)) => return Err(AnthropicError::new(error)),
            None => break,
        }
    }
    for event in decoder.push(&[], true) {
        process_sse_event(
            event,
            sender,
            model,
            context,
            is_oauth,
            compat,
            output,
            &mut usage_model,
            &mut blocks,
            &mut saw_message_start,
            &mut saw_message_stop,
        )?;
    }
    if saw_message_start && !saw_message_stop {
        return Err(AnthropicError::new(
            "Anthropic stream ended before message_stop",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_sse_event(
    event: ServerSentEvent,
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    is_oauth: bool,
    compat: &ResolvedCompat,
    output: &mut AssistantMessage,
    usage_model: &mut Model,
    blocks: &mut BTreeMap<usize, ActiveBlock>,
    saw_message_start: &mut bool,
    saw_message_stop: &mut bool,
) -> Result<(), AnthropicError> {
    if event.event.as_deref() == Some("error") {
        return Err(AnthropicError::new(event.data));
    }
    let event_name = event.event.as_deref().unwrap_or_default();
    if !matches!(
        event_name,
        "message_start"
            | "message_delta"
            | "message_stop"
            | "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
    ) {
        return Ok(());
    }
    let value = parse_json_with_repair::<Value>(&event.data).map_err(|error| {
        AnthropicError::new(format!(
            "Could not parse Anthropic SSE event {}: {}; data={}; raw={}",
            event_name,
            error,
            event.data,
            event.raw.join("\\n")
        ))
    })?;
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            *saw_message_start = true;
            process_message_start(&value, model, compat, output, usage_model);
        }
        Some("message_stop") => *saw_message_stop = true,
        Some("content_block_start") => {
            process_content_block_start(&value, sender, context, is_oauth, output, blocks)?
        }
        Some("content_block_delta") => {
            process_content_block_delta(&value, sender, output, blocks)?;
        }
        Some("content_block_stop") => {
            process_content_block_stop(&value, sender, output, blocks)?;
        }
        Some("message_delta") => process_message_delta(&value, output, usage_model)?,
        _ => {}
    }
    Ok(())
}

fn process_message_start(
    event: &Value,
    model: &Model,
    compat: &ResolvedCompat,
    output: &mut AssistantMessage,
    usage_model: &mut Model,
) {
    let Some(message) = event.get("message") else {
        return;
    };
    output.response_id = message.get("id").and_then(Value::as_str).map(Into::into);
    if let Some(response_model) = message.get("model").and_then(Value::as_str) {
        output.model = response_model.into();
    }
    if output.model == model.id {
        usage_model.clone_from(model);
    } else if let Some(fallback) = compat
        .allowed_fallback_models
        .iter()
        .find(|fallback| fallback.provider == model.provider && output.model == fallback.model)
    {
        usage_model.clone_from(model);
        usage_model.id = output
            .model
            .to_utf8()
            .expect("provider response model originated as UTF-8");
        usage_model.cost.clone_from(&fallback.cost);
    } else {
        usage_model.clone_from(model);
    }
    let usage = message.get("usage").unwrap_or(&Value::Null);
    output.usage.input = js_or_zero(usage.get("input_tokens"));
    output.usage.output = js_or_zero(usage.get("output_tokens"));
    output.usage.cache_read = js_or_zero(usage.get("cache_read_input_tokens"));
    output.usage.cache_write = js_or_zero(usage.get("cache_creation_input_tokens"));
    output.usage.cache_write_1h =
        Some(js_or_zero(usage.get("cache_creation").and_then(
            |creation| creation.get("ephemeral_1h_input_tokens"),
        )));
    update_usage_totals(usage_model, output);
}

fn js_or_zero(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or_default()
}

fn process_content_block_start(
    event: &Value,
    sender: &AssistantStreamSender,
    context: &Context,
    is_oauth: bool,
    output: &mut AssistantMessage,
    blocks: &mut BTreeMap<usize, ActiveBlock>,
) -> Result<(), AnthropicError> {
    let Some(wire_index) = event.get("index").and_then(Value::as_u64) else {
        return Ok(());
    };
    let wire_index = wire_index as usize;
    let Some(block) = event.get("content_block") else {
        return Ok(());
    };
    let content_index = output.content.len();
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            output.content.push(AssistantContent::Text(TextContent::new(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )));
            blocks.insert(wire_index, ActiveBlock::Text { content_index });
            sender
                .send(AssistantMessageEvent::TextStart {
                    content_index: content_index as f64,
                    partial: Arc::new(output.clone()),
                })
                .map_err(AnthropicError::display)?;
        }
        Some("thinking") => {
            let thinking = block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut content = ThinkingContent::new(&thinking);
            content.thinking_signature = Some(signature.clone().into());
            output.content.push(AssistantContent::Thinking(content));
            blocks.insert(wire_index, ActiveBlock::Thinking { content_index });
            sender
                .send(AssistantMessageEvent::ThinkingStart {
                    content_index: content_index as f64,
                    partial: Arc::new(output.clone()),
                })
                .map_err(AnthropicError::display)?;
        }
        Some("redacted_thinking") => {
            let signature = block.get("data").and_then(Value::as_str).map(str::to_owned);
            let mut content = ThinkingContent::new("[Reasoning redacted]");
            content.thinking_signature = signature.clone().map(Into::into);
            content.redacted = Some(true);
            output.content.push(AssistantContent::Thinking(content));
            blocks.insert(wire_index, ActiveBlock::Thinking { content_index });
            sender
                .send(AssistantMessageEvent::ThinkingStart {
                    content_index: content_index as f64,
                    partial: Arc::new(output.clone()),
                })
                .map_err(AnthropicError::display)?;
        }
        Some("tool_use") => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let wire_name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = if is_oauth {
                from_claude_code_name(wire_name, context.tools.as_deref())
            } else {
                wire_name.to_owned()
            };
            let arguments = block
                .get("input")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            output
                .content
                .push(AssistantContent::ToolCall(ToolCall::new(
                    &id,
                    &name,
                    crate::types::JsonObject::try_from(arguments).unwrap_or_default(),
                )));
            blocks.insert(
                wire_index,
                ActiveBlock::ToolCall {
                    content_index,
                    partial_json: String::new(),
                },
            );
            sender
                .send(AssistantMessageEvent::ToolCallStart {
                    content_index: content_index as f64,
                    partial: Arc::new(output.clone()),
                })
                .map_err(AnthropicError::display)?;
        }
        _ => {}
    }
    Ok(())
}

fn process_content_block_delta(
    event: &Value,
    sender: &AssistantStreamSender,
    output: &mut AssistantMessage,
    blocks: &mut BTreeMap<usize, ActiveBlock>,
) -> Result<(), AnthropicError> {
    let Some(wire_index) = event.get("index").and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some(delta) = event.get("delta") else {
        return Ok(());
    };
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            let Some(ActiveBlock::Text { content_index }) = blocks.get(&(wire_index as usize))
            else {
                return Ok(());
            };
            let text = delta
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(AssistantContent::Text(block)) = output.content.get_mut(*content_index) {
                block.text.push_str(text);
                sender
                    .send(AssistantMessageEvent::TextDelta {
                        content_index: *content_index as f64,
                        delta: text.into(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
        Some("thinking_delta") => {
            let Some(ActiveBlock::Thinking { content_index }) = blocks.get(&(wire_index as usize))
            else {
                return Ok(());
            };
            let thinking = delta
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(*content_index)
            {
                block.thinking.push_str(thinking);
                sender
                    .send(AssistantMessageEvent::ThinkingDelta {
                        content_index: *content_index as f64,
                        delta: thinking.into(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
        Some("input_json_delta") => {
            let Some(ActiveBlock::ToolCall {
                content_index,
                partial_json,
            }) = blocks.get_mut(&(wire_index as usize))
            else {
                return Ok(());
            };
            let partial = delta
                .get("partial_json")
                .and_then(Value::as_str)
                .unwrap_or_default();
            partial_json.push_str(partial);
            if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(*content_index)
            {
                block.arguments =
                    crate::utils::json_parse::parse_streaming_json_object(Some(partial_json));
                sender
                    .send(AssistantMessageEvent::ToolCallDelta {
                        content_index: *content_index as f64,
                        delta: partial.into(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
        Some("signature_delta") => {
            let Some(ActiveBlock::Thinking { content_index }) = blocks.get(&(wire_index as usize))
            else {
                return Ok(());
            };
            let signature = delta
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(*content_index)
            {
                block
                    .thinking_signature
                    .get_or_insert_with(Default::default)
                    .push_str(signature);
            }
        }
        _ => {}
    }
    Ok(())
}

fn process_content_block_stop(
    event: &Value,
    sender: &AssistantStreamSender,
    output: &mut AssistantMessage,
    blocks: &mut BTreeMap<usize, ActiveBlock>,
) -> Result<(), AnthropicError> {
    let Some(wire_index) = event.get("index").and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some(block) = blocks.remove(&(wire_index as usize)) else {
        return Ok(());
    };
    match block {
        ActiveBlock::Text { content_index } => {
            if let Some(AssistantContent::Text(block)) = output.content.get(content_index) {
                sender
                    .send(AssistantMessageEvent::TextEnd {
                        content_index: content_index as f64,
                        content: block.text.clone(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
        ActiveBlock::Thinking { content_index } => {
            if let Some(AssistantContent::Thinking(block)) = output.content.get(content_index) {
                sender
                    .send(AssistantMessageEvent::ThinkingEnd {
                        content_index: content_index as f64,
                        content: block.thinking.clone(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
        ActiveBlock::ToolCall {
            content_index,
            partial_json,
        } => {
            if let Some(AssistantContent::ToolCall(block)) = output.content.get_mut(content_index) {
                block.arguments =
                    crate::utils::json_parse::parse_streaming_json_object(Some(&partial_json));
                sender
                    .send(AssistantMessageEvent::ToolCallEnd {
                        content_index: content_index as f64,
                        tool_call: block.clone(),
                        partial: Arc::new(output.clone()),
                    })
                    .map_err(AnthropicError::display)?;
            }
        }
    }
    Ok(())
}

fn process_message_delta(
    event: &Value,
    output: &mut AssistantMessage,
    usage_model: &Model,
) -> Result<(), AnthropicError> {
    if let Some(stop_reason) = event
        .get("delta")
        .and_then(|delta| delta.get("stop_reason"))
        .and_then(Value::as_str)
    {
        output.raw_stop_reason = Some(stop_reason.into());
        let (reason, message) = map_stop_reason(
            stop_reason,
            event
                .get("delta")
                .and_then(|delta| delta.get("stop_details")),
        )?;
        output.stop_reason = reason;
        if message.is_some() {
            output.error_message = message.map(Into::into);
        }
    }
    if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
        update_usage_value(usage, "input_tokens", &mut output.usage.input);
        update_usage_value(usage, "output_tokens", &mut output.usage.output);
        update_usage_value(
            usage,
            "cache_read_input_tokens",
            &mut output.usage.cache_read,
        );
        update_usage_value(
            usage,
            "cache_creation_input_tokens",
            &mut output.usage.cache_write,
        );
        if let Some(thinking) = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .filter(|value| !value.is_null())
        {
            output.usage.reasoning = Some(thinking.as_f64().unwrap_or_default());
        }
        update_usage_totals(usage_model, output);
    }
    Ok(())
}

fn update_usage_value(usage: &Value, key: &str, target: &mut f64) {
    if let Some(value) = usage.get(key).filter(|value| !value.is_null()) {
        *target = value.as_f64().unwrap_or_default();
    }
}

fn update_usage_totals(model: &Model, output: &mut AssistantMessage) {
    output.usage.total_tokens = output.usage.input
        + output.usage.output
        + output.usage.cache_read
        + output.usage.cache_write;
    calculate_cost(model, &mut output.usage);
}

fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), AnthropicError> {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => Ok((
            StopReason::Error,
            Some(
                stop_details
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .filter(|explanation| !explanation.is_empty())
                    .unwrap_or("The model refused to complete the request")
                    .to_owned(),
            ),
        )),
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_owned()),
        )),
        _ => Err(AnthropicError::new(format!(
            "Unhandled stop reason: {reason}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AnthropicMessagesCompat, AssistantRole, ConstrainedSamplingConfig, ImageContent,
        ModelCompat, ModelCost, ModelCostRates, ModelInput, ProviderHttpRequest, StrictPreference,
        ToolConstrainedSampling, ToolResultRole, UserMessage, UserRole,
    };
    use futures::future::BoxFuture;
    use std::sync::{Mutex, PoisonError};

    fn model() -> Model {
        Model {
            id: "claude-sonnet-4-5".to_owned(),
            name: "Claude Sonnet".to_owned(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://example.test".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                },
                tiers: None,
            },
            context_window: 200_000.0,
            max_tokens: 64_000.0,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn user(text: &str) -> Message {
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text((text.to_owned()).into()),
            timestamp: 1.0,
        }))
    }

    fn context() -> Context {
        Context {
            system_prompt: Some(("system".to_owned()).into()),
            messages: vec![user("hello")],
            tools: None,
        }
    }

    #[derive(Clone)]
    struct StaticFetch {
        requests: Arc<Mutex<Vec<ProviderHttpRequest>>>,
        status: u16,
        headers: BTreeMap<String, String>,
        chunks: Vec<Vec<u8>>,
        has_body: bool,
    }

    impl StaticFetch {
        fn sse(body: &str) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                status: 200,
                headers: BTreeMap::from([("x-test".to_owned(), "yes".to_owned())]),
                chunks: vec![body.as_bytes().to_vec()],
                has_body: true,
            }
        }
    }

    impl FetchFunction for StaticFetch {
        fn fetch(
            &self,
            request: ProviderHttpRequest,
        ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            let status = self.status;
            let headers = self.headers.clone();
            let chunks = self.chunks.clone();
            let has_body = self.has_body;
            Box::pin(async move {
                Ok(ProviderHttpResponse {
                    status,
                    status_text: "OK".to_owned(),
                    headers,
                    body: has_body.then(|| {
                        futures::stream::iter(chunks.into_iter().map(Ok)).boxed()
                            as ProviderBodyStream
                    }),
                })
            })
        }
    }

    #[derive(Clone)]
    struct InjectedClient {
        fetch: StaticFetch,
    }

    impl AnthropicMessagesClient for InjectedClient {
        fn messages_url(&self) -> Option<String> {
            Some("https://vertex.example.test/custom/messages".to_owned())
        }

        fn headers(&self) -> ProviderHeaders {
            ProviderHeaders::from([(
                "Authorization".to_owned(),
                Some("Bearer injected".to_owned()),
            )])
        }

        fn fetch(&self) -> Option<Arc<dyn FetchFunction>> {
            Some(Arc::new(self.fetch.clone()))
        }
    }

    fn sse_event(name: &str, value: Value) -> String {
        format!("event: {name}\ndata: {value}\n\n")
    }

    fn successful_sse() -> String {
        [
            sse_event(
                "message_start",
                json!({
                    "type":"message_start",
                    "message":{
                        "id":"msg_1",
                        "model":"claude-sonnet-4-5",
                        "usage":{
                            "input_tokens":10,
                            "output_tokens":1,
                            "cache_read_input_tokens":2,
                            "cache_creation_input_tokens":3,
                            "cache_creation":{"ephemeral_1h_input_tokens":2}
                        }
                    }
                }),
            ),
            sse_event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"H"}}),
            ),
            sse_event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"i"}}),
            ),
            sse_event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            sse_event(
                "message_delta",
                json!({
                    "type":"message_delta",
                    "delta":{"stop_reason":"end_turn"},
                    "usage":{"output_tokens":4,"output_tokens_details":{"thinking_tokens":2}}
                }),
            ),
            sse_event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat()
    }

    /// Ports pi `test/anthropic-sse-parsing.test.ts:33-281` and
    /// `test/anthropic-cache-write-1h-cost.test.ts:8-85` without network access.
    #[tokio::test]
    async fn streams_text_usage_reasoning_and_long_cache_cost() {
        let fetch = StaticFetch::sse(&successful_sse());
        let requests = fetch.requests.clone();
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        let events = stream(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(matches!(
            events.get(1),
            Some(AssistantMessageEvent::TextStart {
                content_index: 0.0,
                ..
            })
        ));
        let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
            panic!("missing done event: {events:?}");
        };
        assert_eq!(message.response_id.as_deref(), Some("msg_1"));
        assert_eq!(message.usage.input, 10.0);
        assert_eq!(message.usage.output, 4.0);
        assert_eq!(message.usage.cache_read, 2.0);
        assert_eq!(message.usage.cache_write, 3.0);
        assert_eq!(message.usage.cache_write_1h, Some(2.0));
        assert_eq!(message.usage.reasoning, Some(2.0));
        assert_eq!(message.usage.total_tokens, 19.0);
        assert_eq!(message.usage.cost.cache_write, 0.000_015_75);
        let request = &requests.lock().unwrap_or_else(PoisonError::into_inner)[0];
        assert_eq!(request.url, "https://example.test/v1/messages");
        assert_eq!(
            request.headers.get("x-api-key").map(String::as_str),
            Some("key")
        );
    }

    /// Pins pi `src/api/anthropic-messages.ts:611-768,1365-1390` where pi has
    /// no hermetic unit test for the complete thinking/tool event sequence.
    #[tokio::test]
    async fn streams_thinking_redaction_tool_input_and_stop_reasons() {
        let body = [
            sse_event(
                "message_start",
                json!({
                    "type":"message_start",
                    "message":{"id":"msg_blocks","model":"claude-sonnet-4-5","usage":{"input_tokens":1,"output_tokens":0}}
                }),
            ),
            sse_event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"A","signature":"sig-"}}),
            ),
            sse_event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"B"}}),
            ),
            sse_event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"tail"}}),
            ),
            sse_event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            sse_event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            ),
            sse_event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            sse_event(
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}),
            ),
            sse_event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"value\":"}}),
            ),
            sse_event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
            ),
            sse_event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            sse_event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
            ),
            sse_event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(StaticFetch::sse(&body)));
        let events = stream(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "B")));
        assert!(events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::ToolCallDelta { delta, .. } if delta == "1}"
        )));
        let Some(AssistantMessageEvent::Done { message, reason }) = events.last() else {
            panic!("missing done: {events:?}");
        };
        assert_eq!(*reason, SuccessfulStopReason::ToolUse);
        let AssistantContent::Thinking(thinking) = &message.content[0] else {
            panic!("missing thinking");
        };
        assert_eq!(thinking.thinking, "AB");
        assert_eq!(thinking.thinking_signature.as_deref(), Some("sig-tail"));
        let AssistantContent::Thinking(redacted) = &message.content[1] else {
            panic!("missing redacted thinking");
        };
        assert_eq!(redacted.redacted, Some(true));
        assert_eq!(redacted.thinking_signature.as_deref(), Some("opaque"));
        let AssistantContent::ToolCall(call) = &message.content[2] else {
            panic!("missing tool call");
        };
        assert_eq!(
            call.arguments,
            JsonObject::try_from(json!({"value":1})).expect("object arguments")
        );

        assert_eq!(
            map_stop_reason("pause_turn", None).expect("pause").0,
            StopReason::Stop
        );
        assert_eq!(
            map_stop_reason("stop_sequence", None).expect("stop").0,
            StopReason::Stop
        );
        assert_eq!(
            map_stop_reason("refusal", Some(&json!({"explanation":"no"}))).expect("refusal"),
            (StopReason::Error, Some("no".to_owned()))
        );
        assert_eq!(
            map_stop_reason("refusal", Some(&json!({"explanation":""}))).expect("refusal"),
            (
                StopReason::Error,
                Some("The model refused to complete the request".to_owned())
            )
        );
        assert_eq!(
            map_stop_reason("sensitive", None).expect("sensitive"),
            (
                StopReason::Error,
                Some("Provider stopped with: sensitive".to_owned())
            )
        );
        assert_eq!(
            map_stop_reason("future", None)
                .expect_err("unknown stop")
                .message,
            "Unhandled stop reason: future"
        );

        let (sender, _events) = AssistantMessageEventStream::channel();
        let mut malformed_output = pending_message(&model());
        let mut malformed_blocks = BTreeMap::new();
        process_content_block_start(
            &json!({
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"redacted_thinking"}
            }),
            &sender,
            &context(),
            false,
            &mut malformed_output,
            &mut malformed_blocks,
        )
        .expect("redacted block");
        let AssistantContent::Thinking(redacted) = &malformed_output.content[0] else {
            panic!("missing malformed redacted block");
        };
        assert_eq!(redacted.thinking_signature, None);
    }

    /// Ports pi `test/fetch-option.test.ts:61-70` and pins
    /// `src/api/anthropic-messages.ts:565-584` request/response hook ordering.
    #[tokio::test]
    async fn stream_simple_uses_custom_fetch_and_runs_payload_and_response_hooks() {
        let mut fetch = StaticFetch::sse(&successful_sse());
        fetch.status = 201;
        fetch
            .headers
            .insert("x-provider-response".to_owned(), "real".to_owned());
        let requests = fetch.requests.clone();
        let observed_response = Arc::new(Mutex::new(None::<ProviderResponse>));
        let response_hook = observed_response.clone();
        let mut options = SimpleStreamOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        options.stream.request.on_payload = Some(Arc::new(|mut payload, _| {
            Box::pin(async move {
                payload
                    .as_object_mut()
                    .expect("payload object")
                    .insert("hook_field".to_owned(), json!("present"));
                Ok(Some(payload))
            })
        }));
        options.stream.request.on_response = Some(Arc::new(move |response, _| {
            let observed = response_hook.clone();
            Box::pin(async move {
                *observed.lock().unwrap_or_else(PoisonError::into_inner) = Some(response);
                Ok(())
            })
        }));

        let events = stream_simple(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(requests.len(), 1);
        let body: Value =
            serde_json::from_slice(requests[0].body.as_deref().expect("request body"))
                .expect("request body");
        assert_eq!(body["hook_field"], "present");
        let response = observed_response
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let response = response.as_ref().expect("response hook");
        assert_eq!(response.status, 201.0);
        assert_eq!(
            response
                .headers
                .get("x-provider-response")
                .map(String::as_str),
            Some("real")
        );
    }

    /// Pins pi `src/types.ts:350-380` and
    /// `src/api/anthropic-messages.ts:1201-1257`: payload hooks receive the
    /// actual ECMAScript object, while the HTTP JSON source escapes isolated
    /// UTF-16 code units without reserving user object keys.
    #[tokio::test]
    async fn payload_hook_and_wire_preserve_surrogates_and_sentinel_shaped_user_objects() {
        let fetch = StaticFetch::sse(&successful_sse());
        let requests = fetch.requests.clone();
        let observed = Arc::new(Mutex::new(None::<JsonValue>));
        let hook_observed = Arc::clone(&observed);

        let mut arguments = JsonObject::new();
        arguments.insert("\0agentprism.ecma-json.object", r#"{"ordinary":true}"#);
        let mut thinking = ThinkingContent::new("reasoning");
        thinking.thinking_signature = Some(crate::types::JsString::from_utf16(vec![0xd83d]));
        let call = ToolCall::new(
            crate::types::JsString::from_utf16(vec![0xde00]),
            crate::types::JsString::from_utf16(vec![0xd83d]),
            arguments,
        );
        let current_model = model();
        let mut assistant =
            AssistantMessage::pending("anthropic-messages", "anthropic", &current_model.id, 1.0);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            AssistantContent::Thinking(thinking),
            AssistantContent::ToolCall(call),
        ];
        let replay = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant))],
            tools: None,
        };

        let mut options = SimpleStreamOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        options.stream.request.on_payload = Some(Arc::new(move |payload, _| {
            let observed = Arc::clone(&hook_observed);
            Box::pin(async move {
                *observed.lock().unwrap_or_else(PoisonError::into_inner) = Some(payload.clone());
                Ok(None)
            })
        }));

        let events = stream_simple(&current_model, &replay, options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));

        let observed = observed.lock().unwrap_or_else(PoisonError::into_inner);
        let payload = observed.as_ref().expect("payload hook");
        assert_eq!(
            payload["messages"][0]["content"][0]["signature"]
                .as_str()
                .expect("thinking signature")
                .as_utf16(),
            &[0xd83d]
        );
        assert_eq!(
            payload["messages"][0]["content"][1]["id"]
                .as_str()
                .expect("tool id")
                .as_utf16(),
            &[0xde00]
        );
        assert_eq!(
            payload["messages"][0]["content"][1]["name"]
                .as_str()
                .expect("tool name")
                .as_utf16(),
            &[0xd83d]
        );
        assert_eq!(
            payload["messages"][0]["content"][1]["input"]["\0agentprism.ecma-json.object"],
            r#"{"ordinary":true}"#
        );
        drop(observed);

        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        let wire =
            std::str::from_utf8(requests[0].body.as_deref().expect("Anthropic request body"))
                .expect("UTF-8 JSON source");
        assert!(wire.contains(r#""signature":"\ud83d""#));
        assert!(wire.contains(r#""id":"\ude00""#));
        assert!(wire.contains(r#""name":"\ud83d""#));
        assert!(wire.contains(r#""\u0000agentprism.ecma-json.object":"{\"ordinary\":true}""#));
    }

    /// Ports the pre-built/alternative client seam at pi
    /// `src/api/anthropic-messages.ts:267-271,528-564`.
    #[tokio::test]
    async fn injected_client_owns_url_headers_and_transport() {
        fn assert_adk_client_implements_seam<T: AnthropicMessagesClient>() {}
        assert_adk_client_implements_seam::<Anthropic>();

        let fetch = StaticFetch::sse(&successful_sse());
        let requests = fetch.requests.clone();
        let mut options = AnthropicOptions {
            client: Some(Arc::new(InjectedClient { fetch })),
            ..Default::default()
        };
        options.stream.request.api_key = Some("caller-key".to_owned());
        options.stream.request.headers = Some(ProviderHeaders::from([(
            "x-caller".to_owned(),
            Some("ignored".to_owned()),
        )]));
        let events = stream(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            requests[0].url,
            "https://vertex.example.test/custom/messages"
        );
        assert_eq!(
            requests[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer injected")
        );
        assert!(!requests[0].headers.contains_key("x-api-key"));
        assert!(!requests[0].headers.contains_key("x-caller"));
    }

    /// Ports pi `test/anthropic-auth-token.test.ts:131-215`,
    /// `test/github-copilot-anthropic.test.ts:77-126`, and OAuth shaping at
    /// `src/api/anthropic-messages.ts:924-970`.
    #[test]
    fn api_key_header_oauth_and_copilot_request_headers_match_pi() {
        let mut options = AnthropicOptions::default();
        options.stream.request.headers = Some(ProviderHeaders::from([
            (
                "Authorization".to_owned(),
                Some("Bearer gateway-token".to_owned()),
            ),
            ("User-Agent".to_owned(), Some("custom-client".to_owned())),
        ]));
        assert!(
            assert_request_auth("anthropic", None, options.stream.request.headers.as_ref()).is_ok()
        );
        let headers = create_headers(
            &model(),
            &context(),
            None,
            false,
            &options,
            &get_anthropic_compat(&model()),
        );
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer gateway-token")
        );
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("custom-client")
        );
        assert!(!headers.contains_key("x-api-key"));
        assert!(!headers["anthropic-beta"].contains("oauth-2025-04-20"));

        let oauth_headers = create_headers(
            &model(),
            &context(),
            Some("sk-ant-oat-test"),
            true,
            &AnthropicOptions::default(),
            &get_anthropic_compat(&model()),
        );
        assert_eq!(
            oauth_headers.get("Authorization").map(String::as_str),
            Some("Bearer sk-ant-oat-test")
        );
        assert_eq!(oauth_headers.get("x-app").map(String::as_str), Some("cli"));
        assert_eq!(
            oauth_headers.get("user-agent").map(String::as_str),
            Some("claude-cli/2.1.75")
        );
        assert!(!oauth_headers.contains_key("User-Agent"));
        assert_eq!(
            oauth_headers.get("anthropic-beta").map(String::as_str),
            Some("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14")
        );
        let oauth_params =
            build_params(&model(), &context(), true, &AnthropicOptions::default()).expect("params");
        assert_eq!(oauth_params["system"][0]["text"], CLAUDE_CODE_IDENTITY);

        let mut copilot = model();
        copilot.provider = "github-copilot".into();
        copilot.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            force_adaptive_thinking: Some(true),
            supports_eager_tool_input_streaming: Some(false),
            ..Default::default()
        }));
        copilot.headers = Some(indexmap::IndexMap::from([
            (
                "User-Agent".to_owned(),
                "GitHubCopilotChat/0.35.0".to_owned(),
            ),
            (
                "Copilot-Integration-Id".to_owned(),
                "vscode-chat".to_owned(),
            ),
        ]));
        let headers = create_headers(
            &copilot,
            &context(),
            Some("tid_session"),
            false,
            &AnthropicOptions::default(),
            &get_anthropic_compat(&copilot),
        );
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer tid_session")
        );
        assert_eq!(headers.get("X-Initiator").map(String::as_str), Some("user"));
        assert_eq!(
            headers.get("Openai-Intent").map(String::as_str),
            Some("conversation-edits")
        );
        assert_eq!(
            headers.get("Copilot-Integration-Id").map(String::as_str),
            Some("vscode-chat")
        );
        assert!(
            !headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("interleaved-thinking"))
        );
        assert!(
            !headers
                .get("anthropic-beta")
                .is_some_and(|value| value.contains("fine-grained-tool-streaming"))
        );
    }

    /// Ports pi `test/anthropic-eager-tool-input-compat.test.ts:123-165`.
    #[test]
    fn eager_legacy_beta_and_strict_tool_schemas_follow_compat() {
        let legacy = Tool {
            name: "lookup".to_owned(),
            description: "Look up a value".to_owned(),
            parameters: json!({
                "type":"object",
                "title":"LookupInput",
                "additionalProperties":false,
                "properties":{"value":{"type":"string"}},
                "required":["value"]
            }),
            constrained_sampling: None,
        };
        let tool_context = Context {
            system_prompt: None,
            messages: vec![user("Use the tool")],
            tools: Some(vec![legacy.clone()]),
        };
        let params = build_params(&model(), &tool_context, false, &AnthropicOptions::default())
            .expect("params");
        assert_eq!(params["tools"][0]["eager_input_streaming"], true);
        assert_eq!(
            params["tools"][0]["input_schema"],
            json!({
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"]
            })
        );

        let mut legacy_model = model();
        legacy_model.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            supports_eager_tool_input_streaming: Some(false),
            ..Default::default()
        }));
        let no_interleaved = AnthropicOptions {
            interleaved_thinking: Some(false),
            ..Default::default()
        };
        let params =
            build_params(&legacy_model, &tool_context, false, &no_interleaved).expect("params");
        assert!(params["tools"][0].get("eager_input_streaming").is_none());
        let headers = create_headers(
            &legacy_model,
            &tool_context,
            Some("key"),
            false,
            &no_interleaved,
            &get_anthropic_compat(&legacy_model),
        );
        assert_eq!(
            headers.get("anthropic-beta").map(String::as_str),
            Some(FINE_GRAINED_TOOL_STREAMING_BETA)
        );

        let strict = Tool {
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::JsonSchema {
                    strict: StrictPreference::Prefer,
                },
            )),
            parameters: json!({
                "type":"object",
                "title":"StrictLookupInput",
                "properties":{
                    "value":{"type":"string"},
                    "optional":{"type":"number"}
                },
                "required":["value"]
            }),
            ..legacy
        };
        let strict_context = Context {
            tools: Some(vec![strict]),
            ..tool_context
        };
        let mut strict_model = model();
        strict_model.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            supports_strict_tools: Some(true),
            ..Default::default()
        }));
        let params = build_params(
            &strict_model,
            &strict_context,
            false,
            &AnthropicOptions::default(),
        )
        .expect("params");
        assert_eq!(params["tools"][0]["strict"], true);
        assert_eq!(
            params["tools"][0]["input_schema"]["title"],
            "StrictLookupInput"
        );
        assert_eq!(
            params["tools"][0]["input_schema"]["required"],
            json!(["value", "optional"])
        );
        assert_eq!(
            params["tools"][0]["input_schema"]["properties"]["optional"],
            json!({"anyOf":[{"type":"number"},{"type":"null"}]})
        );
    }

    /// Pins pi `src/api/anthropic-messages.ts:179-180,575-610`: fallback
    /// requests advertise the beta and responses use the selected model's cost.
    #[tokio::test]
    async fn fallback_response_uses_declared_cost_and_beta() {
        let fallback_cost = ModelCost {
            rates: ModelCostRates {
                input: 30.0,
                output: 150.0,
                cache_read: 3.0,
                cache_write: 37.5,
            },
            tiers: None,
        };
        let mut fallback_model = model();
        fallback_model.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            allowed_fallback_models: Some(vec![AnthropicAllowedFallbackModel {
                provider: "anthropic".into(),
                model: "claude-fallback".to_owned(),
                cost: fallback_cost,
            }]),
            ..Default::default()
        }));
        let body = successful_sse().replace("claude-sonnet-4-5", "claude-fallback");
        let fetch = StaticFetch::sse(&body);
        let requests = fetch.requests.clone();
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        let events = stream(&fallback_model, &context(), options)
            .collect::<Vec<_>>()
            .await;
        let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
            panic!("missing done event: {events:?}");
        };
        assert_eq!(message.model, "claude-fallback");
        assert!((message.usage.cost.input - 0.0003).abs() < f64::EPSILON);
        assert!((message.usage.cost.output - 0.0006).abs() < f64::EPSILON);
        assert!((message.usage.cost.cache_read - 0.000006).abs() < f64::EPSILON);
        assert!((message.usage.cost.cache_write - 0.0001575).abs() < f64::EPSILON);
        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(requests[0].headers["anthropic-beta"].contains(SERVER_SIDE_FALLBACK_BETA));
        let payload: Value =
            serde_json::from_slice(requests[0].body.as_deref().expect("payload body"))
                .expect("payload");
        assert_eq!(payload["fallbacks"], json!([{"model":"claude-fallback"}]));
    }

    /// Ports pi `test/anthropic-sse-parsing.test.ts:283-423` parser edge cases.
    #[test]
    fn sse_decoder_handles_chunked_utf8_crlf_multiline_comments_and_eof() {
        let mut decoder = SseDecoder::default();
        let bytes = "évent: ignored\r\n:comment\r\nevent: message_stop\r\ndata: {\"type\":\r\ndata: \"message_stop\"}\r\n"
            .as_bytes();
        assert!(decoder.push(&bytes[..1], false).is_empty());
        assert!(decoder.push(&bytes[1..7], false).is_empty());
        let events = decoder.push(&bytes[7..], true);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_stop"));
        assert_eq!(events[0].data, "{\"type\":\n\"message_stop\"}");
        assert!(events[0].raw.iter().any(|line| line == ":comment"));
    }

    /// Ports pi `test/anthropic-sse-parsing.test.ts:43-139`: malformed JSON strings are repaired.
    #[test]
    fn recognized_sse_json_is_repaired_but_unknown_events_are_ignored() {
        let raw = ServerSentEvent {
            event: Some("content_block_delta".to_owned()),
            data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#.to_owned(),
            raw: vec!["data: malformed".to_owned()],
        };
        let repaired = parse_json_with_repair::<Value>(&raw.data).expect("repair");
        assert_eq!(
            repaired["delta"]["partial_json"],
            Value::String("{\"path\":\"A\\H\",\"text\":\"col1\tcol2\"}".to_owned())
        );
        let unknown = ServerSentEvent {
            event: Some("ping".to_owned()),
            data: "not-json".to_owned(),
            raw: vec![],
        };
        let (sender, _) = AssistantMessageEventStream::channel();
        let mut output = pending_message(&model());
        assert!(
            process_sse_event(
                unknown,
                &sender,
                &model(),
                &context(),
                false,
                &get_anthropic_compat(&model()),
                &mut output,
                &mut model(),
                &mut BTreeMap::new(),
                &mut false,
                &mut false,
            )
            .is_ok()
        );
    }

    /// Ports pi `test/cache-retention.test.ts:52-240` and
    /// `test/anthropic-temperature-compat.test.ts:7-102`.
    #[test]
    fn payload_cache_retention_and_temperature_follow_compat() {
        let mut options = AnthropicOptions::default();
        options.stream.temperature = Some(0.25);
        options.stream.cache_retention = Some(CacheRetention::Long);
        let params = build_params(&model(), &context(), false, &options).expect("params");
        assert_eq!(params["temperature"], json!(0.25));
        assert_eq!(params["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            params["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );

        let mut incompatible = model();
        incompatible.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            supports_long_cache_retention: Some(false),
            supports_temperature: Some(false),
            ..Default::default()
        }));
        let params = build_params(&incompatible, &context(), false, &options).expect("params");
        assert!(params.get("temperature").is_none());
        assert!(params["system"][0]["cache_control"].get("ttl").is_none());

        let short = build_params(&model(), &context(), false, &AnthropicOptions::default())
            .expect("params");
        assert_eq!(
            short["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );

        let mut from_env = AnthropicOptions::default();
        from_env.stream.request.env = Some(ProviderEnv::from([(
            "PI_CACHE_RETENTION".to_owned(),
            "long".to_owned(),
        )]));
        let params = build_params(&model(), &context(), false, &from_env).expect("params");
        assert_eq!(params["system"][0]["cache_control"]["ttl"], "1h");

        let mut affinity_model = model();
        affinity_model.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            send_session_affinity_headers: Some(true),
            ..Default::default()
        }));
        let mut affinity = AnthropicOptions::default();
        affinity.stream.session_id = Some("session-1".to_owned());
        let headers = create_headers(
            &affinity_model,
            &context(),
            Some("key"),
            false,
            &affinity,
            &get_anthropic_compat(&affinity_model),
        );
        assert_eq!(
            headers.get("x-session-affinity").map(String::as_str),
            Some("session-1")
        );

        affinity.stream.cache_retention = Some(CacheRetention::None);
        let params = build_params(&affinity_model, &context(), false, &affinity).expect("params");
        assert!(params["system"][0].get("cache_control").is_none());
        assert!(
            params["messages"][0]["content"]
                .as_str()
                .is_some_and(|content| content == "hello")
        );
        let headers = create_headers(
            &affinity_model,
            &context(),
            Some("key"),
            false,
            &affinity,
            &get_anthropic_compat(&affinity_model),
        );
        assert!(!headers.contains_key("x-session-affinity"));

        let mut thinking = options;
        thinking.thinking_enabled = Some(true);
        let params = build_params(&model(), &context(), false, &thinking).expect("params");
        assert!(params.get("temperature").is_none());
    }

    /// Ports pi `test/anthropic-force-adaptive-thinking.test.ts:8-122`,
    /// `test/anthropic-thinking-disable.test.ts:8-119`, and sampling-option omission.
    #[test]
    fn adaptive_and_disabled_thinking_payloads_are_exact() {
        let mut adaptive = model();
        adaptive.compat = Some(ModelCompat::AnthropicMessages(AnthropicMessagesCompat {
            force_adaptive_thinking: Some(true),
            ..Default::default()
        }));
        let mut options = AnthropicOptions {
            thinking_enabled: Some(true),
            effort: Some(AnthropicEffort::Xhigh),
            ..Default::default()
        };
        options.stream.sampling_params = Some(Map::from_iter([("top_p".to_owned(), json!(0.9))]));
        let params = build_params(&adaptive, &context(), false, &options).expect("params");
        assert_eq!(
            params["thinking"],
            json!({"type":"adaptive","display":"summarized"})
        );
        assert_eq!(params["output_config"], json!({"effort":"xhigh"}));
        assert!(params.get("top_p").is_none());

        let disabled = build_params(
            &model(),
            &context(),
            false,
            &AnthropicOptions {
                thinking_enabled: Some(false),
                ..Default::default()
            },
        )
        .expect("params");
        assert_eq!(disabled["thinking"], json!({"type":"disabled"}));

        let legacy = build_params(
            &model(),
            &context(),
            false,
            &AnthropicOptions {
                thinking_enabled: Some(true),
                thinking_budget_tokens: Some(0.0),
                tool_choice: Some(AnthropicToolChoice::Mode(AnthropicToolChoiceMode::Any)),
                ..Default::default()
            },
        )
        .expect("params");
        assert_eq!(legacy["thinking"]["budget_tokens"], 1_024);
        assert_eq!(legacy["tool_choice"], json!({"type":"any"}));

        let forced = build_params(
            &model(),
            &context(),
            false,
            &AnthropicOptions {
                tool_choice: Some(AnthropicToolChoice::Tool {
                    kind: AnthropicToolChoiceToolType::Tool,
                    name: "lookup".to_owned(),
                }),
                ..Default::default()
            },
        )
        .expect("params");
        assert_eq!(
            forced["tool_choice"],
            json!({"type":"tool","name":"lookup"})
        );
    }

    /// Ports pi `test/anthropic-empty-thinking-signature-compat.test.ts:8-107`.
    #[test]
    fn empty_thinking_signature_downgrades_unless_compat_allows_it() {
        let mut previous =
            AssistantMessage::pending("anthropic-messages", "anthropic", "claude-sonnet-4-5", 1.0);
        previous.role = AssistantRole::Assistant;
        previous.content = vec![AssistantContent::Thinking(ThinkingContent::new("thought"))];
        previous.stop_reason = StopReason::Stop;
        let messages = vec![Message::Assistant(Box::new(previous))];
        let normal = convert_messages(
            &messages,
            false,
            None,
            false,
            &IndexSet::new(),
            &str::to_owned,
        );
        assert_eq!(normal[0]["content"][0]["type"], "text");
        let compatible = convert_messages(
            &messages,
            false,
            None,
            true,
            &IndexSet::new(),
            &str::to_owned,
        );
        assert_eq!(compatible[0]["content"][0]["type"], "thinking");
        assert_eq!(compatible[0]["content"][0]["signature"], "");

        let mut replay =
            AssistantMessage::pending("anthropic-messages", "anthropic", "claude-sonnet-4-5", 1.0);
        replay.role = AssistantRole::Assistant;
        let mut redacted = ThinkingContent::new("[Reasoning redacted]");
        redacted.redacted = Some(true);
        replay.content = vec![
            AssistantContent::Thinking(redacted),
            AssistantContent::ToolCall(ToolCall::new("call", "lookup", JsonObject::new())),
        ];
        replay.stop_reason = StopReason::ToolUse;
        let converted = convert_messages(
            &[Message::Assistant(Box::new(replay))],
            false,
            None,
            false,
            &IndexSet::new(),
            &str::to_owned,
        );
        assert!(converted[0]["content"][0].get("data").is_none());
        assert_eq!(converted[0]["content"][1]["input"], json!({}));
    }

    /// Pins pi `types.ts:372-380` and
    /// `src/api/anthropic-messages.ts:1252-1258`: replay preserves the complete
    /// dynamic argument map; JSON serialization nulls only non-finite leaves.
    #[test]
    fn replay_preserves_dynamic_tool_arguments_around_nonfinite_leaves() {
        let mut arguments = JsonObject::new();
        arguments.insert("before", -1e20_f64);
        arguments.insert("nan", f64::NAN);
        arguments.insert(
            "nested",
            crate::types::JsonValue::Array(vec![true.into(), f64::INFINITY.into(), "after".into()]),
        );
        let mut replay =
            AssistantMessage::pending("anthropic-messages", "anthropic", "claude", 1.0);
        replay.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call", "lookup", arguments,
        ))];
        replay.stop_reason = StopReason::ToolUse;
        let converted = convert_messages(
            &[Message::Assistant(Box::new(replay))],
            false,
            None,
            false,
            &IndexSet::new(),
            &str::to_owned,
        );
        let input = &converted[0]["content"][0]["input"];
        assert_eq!(input["before"].as_f64(), Some(-1e20_f64));
        assert!(input["nan"].as_f64().is_some_and(f64::is_nan));
        assert_eq!(input["nested"][1].as_f64(), Some(f64::INFINITY));
        assert_eq!(
            crate::utils::ecma_json::stringify_provider_json(input),
            r#"{"before":-100000000000000000000,"nan":null,"nested":[true,null,"after"]}"#
        );
    }

    /// Pins pi `src/api/anthropic-messages.ts:1252-1258` at the HTTP body
    /// boundary, including ECMAScript integer-index ordering and lone UTF-16
    /// code units in identifiers, keys, and values.
    #[tokio::test]
    async fn request_wire_replays_lossless_ordered_tool_arguments() {
        let mut arguments = JsonObject::new();
        arguments.insert("10", "ten");
        arguments.insert("2", "two");
        arguments.insert(
            crate::types::JsString::from_utf16(vec![0xd83d]),
            crate::types::JsonValue::String(crate::types::JsString::from_utf16(vec![0xde00])),
        );
        let mut replay =
            AssistantMessage::pending("anthropic-messages", "anthropic", "claude-sonnet-4-5", 1.0);
        replay.content = vec![AssistantContent::ToolCall(ToolCall::new(
            crate::types::JsString::from_utf16(vec![0xd801]),
            crate::types::JsString::from_utf16(vec![0xdc01]),
            arguments,
        ))];
        replay.stop_reason = StopReason::ToolUse;
        let request_context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(replay))],
            tools: None,
        };
        let fetch = StaticFetch::sse(&successful_sse());
        let requests = fetch.requests.clone();
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        let _ = stream(&model(), &request_context, options)
            .collect::<Vec<_>>()
            .await;
        let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
        let wire = std::str::from_utf8(requests[0].body.as_deref().expect("request body"))
            .expect("ASCII JSON escapes");
        assert!(wire.contains(
            r#""id":"\ud801","name":"\udc01","input":{"2":"two","10":"ten","\ud83d":"\ude00"}"#
        ));
    }

    /// Ports the hermetic assertions in pi `test/deferred-tools.test.ts:80-343`.
    #[test]
    fn deferred_references_displace_ordinary_result_content_and_deduplicate() {
        let tools = vec![
            Tool {
                name: "base".to_owned(),
                description: "base".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
            Tool {
                name: "late".to_owned(),
                description: "late".to_owned(),
                parameters: json!({"type":"object"}),
                constrained_sampling: None,
            },
        ];
        let marker = |id: &str| {
            Message::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: id.into(),
                tool_name: "base".into(),
                content: vec![UserContentBlock::Text(TextContent::new("ordinary"))],
                details: None,
                usage: None,
                added_tool_names: Some(vec!["late".into()]),
                is_error: false,
                timestamp: 2.0,
            }))
        };
        let context = Context {
            system_prompt: None,
            messages: vec![marker("a"), marker("b")],
            tools: Some(tools),
        };
        let params =
            build_params(&model(), &context, false, &AnthropicOptions::default()).expect("params");
        assert_eq!(params["tools"][0]["name"], "base");
        assert_eq!(params["tools"][1]["name"], "late");
        assert_eq!(params["tools"][1]["defer_loading"], true);
        let content = params["messages"][0]["content"]
            .as_array()
            .expect("content");
        assert_eq!(content[0]["content"][0]["type"], "tool_reference");
        assert_eq!(content[1]["content"], "ordinary");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "ordinary");
    }

    /// Ports pi `test/anthropic-sse-parsing.test.ts:342-385` terminal checks.
    #[tokio::test]
    async fn missing_message_stop_and_no_body_fail_after_start() {
        let body = successful_sse().replace(
            &sse_event("message_stop", json!({"type":"message_stop"})),
            "",
        );
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(StaticFetch::sse(&body)));
        let events = stream(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
            panic!("missing error");
        };
        assert_eq!(
            error.error_message.as_deref(),
            Some("Anthropic stream ended before message_stop")
        );

        let mut fetch = StaticFetch::sse("");
        fetch.has_body = false;
        let mut options = AnthropicOptions::default();
        options.stream.request.api_key = Some("key".to_owned());
        options.stream.request.fetch = Some(Arc::new(fetch));
        let events = stream(&model(), &context(), options)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
    }

    /// Pins pi `src/api/anthropic-messages.ts:76-165,198-210,1115-1118`.
    #[test]
    fn claude_code_names_and_tool_reference_defaults_match_pi() {
        assert_eq!(to_claude_code_name("read"), "Read");
        assert_eq!(to_claude_code_name("custom"), "custom");
        assert!(default_supports_tool_references(&model()));
        let mut haiku = model();
        haiku.id = "claude-haiku-4-5".to_owned();
        assert!(!default_supports_tool_references(&haiku));
        assert_eq!(normalize_anthropic_tool_call_id("a😀b/"), "a__b_");
        let image_only = convert_content_blocks(&[UserContentBlock::Image(ImageContent::new(
            "AA==",
            "image/png",
        ))]);
        assert_eq!(image_only[0]["text"], "(see attached image)");
    }
}
