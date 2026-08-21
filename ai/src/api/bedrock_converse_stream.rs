//! Amazon Bedrock Converse Stream ⇐ pi `src/api/bedrock-converse-stream.ts`.

use crate::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::api::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
    clamp_reasoning,
};
use crate::api::transform_messages::transform_messages;
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::calculate_cost;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, CacheRetention, Context,
    ErrorStopReason, Message, Model, ModelCompat, ProviderEnv, ProviderResponse,
    SimpleStreamOptions, StopReason, StreamOptions, SuccessfulStopReason, TextContent,
    ThinkingBudgets, ThinkingContent, ThinkingLevel, Tool, ToolCall, ToolChoice, UsageValue,
    UserContent, UserContentBlock,
};
use crate::utils::error_body::trim_javascript_whitespace;
use crate::utils::headers::provider_headers_to_record;
use crate::utils::json_parse::parse_streaming_json;
use crate::utils::node_http_proxy::resolve_http_proxy_url_for_target;
use crate::utils::provider_env::get_provider_env_value;
use base64::Engine as _;
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, PAD_INDIFFERENT, STANDARD};
use futures::FutureExt;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS: usize = 200;
const BROWSER_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    PAD_INDIFFERENT.with_decode_allow_trailing_bits(true),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BedrockToolChoice {
    Mode(BedrockToolChoiceMode),
    Tool {
        #[serde(rename = "type")]
        kind: BedrockToolChoiceToolType,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockToolChoiceMode {
    Auto,
    Any,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockToolChoiceToolType {
    Tool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<BedrockToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<BedrockThinkingDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_metadata: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

impl From<StreamOptions> for BedrockOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BedrockConverseStreamApi;

pub fn bedrock_converse_stream_api() -> BedrockConverseStreamApi {
    BedrockConverseStreamApi
}

impl ProviderStreams for BedrockConverseStreamApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        match options {
            ApiStreamOptions::Base(options) => stream(model, context, options.into()),
            ApiStreamOptions::BedrockConverseStream(options) => stream(model, context, options),
            ApiStreamOptions::AnthropicMessages(_)
            | ApiStreamOptions::GoogleGenerativeAI(_)
            | ApiStreamOptions::GoogleVertex(_)
            | ApiStreamOptions::OpenAICompletions(_)
            | ApiStreamOptions::OpenAIResponses(_)
            | ApiStreamOptions::OpenAICodexResponses(_)
            | ApiStreamOptions::Custom { .. } => terminal_setup_error(
                model,
                "API options variant does not match bedrock-converse-stream",
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

pub fn stream(
    model: &Model,
    context: &Context,
    options: BedrockOptions,
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
    stream(
        model,
        context,
        lower_simple_options(model, context, &options),
    )
}

fn lower_simple_options(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
) -> BedrockOptions {
    let mut result = BedrockOptions {
        stream: build_base_options(model, context, Some(options), None),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => BedrockToolChoice::Mode(BedrockToolChoiceMode::Auto),
            ToolChoice::None => BedrockToolChoice::Mode(BedrockToolChoiceMode::None),
        }),
        ..BedrockOptions::default()
    };
    let Some(reasoning) = options.reasoning else {
        return result;
    };
    result.reasoning = Some(reasoning);
    result.thinking_budgets = options.thinking_budgets.clone();
    if !is_anthropic_claude_model(model) || supports_adaptive_thinking(&model.id, &model.name) {
        return result;
    }

    let adjusted = adjust_max_tokens_for_thinking(
        result.stream.max_tokens.map(|value| value as f64),
        model.max_tokens as f64,
        reasoning,
        options.thinking_budgets.as_ref(),
    );
    let max_tokens = clamp_max_tokens_to_context(model, context, adjusted.max_tokens as u64);
    result.stream.max_tokens = Some(max_tokens);
    let level = clamp_reasoning(Some(reasoning)).expect("reasoning is present");
    let budget = adjusted
        .thinking_budget
        .min(max_tokens.saturating_sub(1_024) as f64);
    let budgets = result.thinking_budgets.get_or_insert_with(Default::default);
    match level {
        ThinkingLevel::Minimal => budgets.minimal = Some(budget),
        ThinkingLevel::Low => budgets.low = Some(budget),
        ThinkingLevel::Medium => budgets.medium = Some(budget),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            budgets.high = Some(budget);
        }
    }
    result
}

fn terminal_setup_error(model: &Model, message: &str) -> AssistantMessageEventStream {
    let mut output = pending_message(model);
    output.stop_reason = StopReason::Error;
    output.error_message = Some(message.to_owned());
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error: output,
    }])
}

fn pending_message(model: &Model) -> AssistantMessage {
    AssistantMessage::pending(
        "bedrock-converse-stream",
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    )
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

async fn run_stream(
    sender: AssistantStreamSender,
    model: Model,
    context: Context,
    options: BedrockOptions,
) {
    let mut output = pending_message(&model);
    let resolution = match resolve_client_configuration(&model, &options) {
        Ok(resolution) => resolution,
        // pi `bedrock-converse-stream.ts:144-235`: client setup precedes its try/catch.
        Err(_) => std::future::pending::<ClientResolution>().await,
    };
    let result = AssertUnwindSafe(run_stream_inner(
        &sender,
        &model,
        &context,
        &options,
        &mut output,
        resolution,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| {
        Err(BedrockError::plain(
            crate::utils::diagnostics::format_panic_payload(panic.as_ref()),
        ))
    });
    if let Err(error) = result {
        let aborted = options
            .stream
            .request
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        output.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(format_bedrock_error(&error));
        if !aborted {
            append_bedrock_failure_diagnostic(
                &mut output,
                &error,
                error.fallback_request_id.as_deref(),
            );
        }
        let _ = sender.send(AssistantMessageEvent::Error {
            reason: if aborted {
                ErrorStopReason::Aborted
            } else {
                ErrorStopReason::Error
            },
            error: output,
        });
    }
}

async fn run_stream_inner(
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &BedrockOptions,
    output: &mut AssistantMessage,
    resolution: ClientResolution,
) -> Result<(), BedrockError> {
    let cache_retention = resolve_cache_retention(
        options.stream.cache_retention,
        options.stream.request.env.as_ref(),
    );
    let mut payload = build_command_input(context, model, options, cache_retention)?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(payload.clone(), model)
            .await
            .map_err(BedrockError::plain)?
    {
        payload = replacement;
    }
    let response_request_id = sdk::run(sender, model, options, output, resolution, payload).await?;

    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(BedrockError::plain("Request was aborted")
            .with_fallback_request_id(response_request_id));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(
            BedrockError::plain("Bedrock stream ended without a stop reason")
                .with_fallback_request_id(response_request_id),
        );
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(BedrockError::plain(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_owned()),
        )
        .with_fallback_request_id(response_request_id));
    }
    let reason = match output.stop_reason {
        StopReason::Stop => SuccessfulStopReason::Stop,
        StopReason::Length => SuccessfulStopReason::Length,
        StopReason::ToolUse => SuccessfulStopReason::ToolUse,
        StopReason::Deferred => SuccessfulStopReason::Deferred,
        StopReason::Pending | StopReason::Error | StopReason::Aborted => {
            return Err(BedrockError::plain("An unknown error occurred")
                .with_fallback_request_id(response_request_id));
        }
    };
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(|_| BedrockError::plain("Assistant event stream receiver was dropped"))?;
    Ok(())
}

fn supports_strict_mode(model: &Model) -> bool {
    matches!(
        model.compat.as_ref(),
        Some(ModelCompat::Bedrock(compat)) if compat.supports_strict_mode.unwrap_or(false)
    )
}

fn build_command_input(
    context: &Context,
    model: &Model,
    options: &BedrockOptions,
    cache_retention: CacheRetention,
) -> Result<Value, BedrockError> {
    let mut result = Map::new();
    result.insert("modelId".to_owned(), Value::String(model.id.clone()));
    result.insert(
        "messages".to_owned(),
        Value::Array(convert_messages(
            context,
            model,
            cache_retention,
            options.stream.request.env.as_ref(),
        )?),
    );
    if let Some(system) = build_system_prompt(
        context.system_prompt.as_deref(),
        model,
        cache_retention,
        options.stream.request.env.as_ref(),
    ) {
        result.insert("system".to_owned(), Value::Array(system));
    }
    let mut inference = Map::new();
    let inference_max_tokens = options
        .stream
        .max_tokens
        .or_else(|| is_anthropic_claude_model(model).then_some(model.max_tokens));
    if let Some(max_tokens) = inference_max_tokens {
        inference.insert("maxTokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(temperature) = options.stream.temperature {
        inference.insert(
            "temperature".to_owned(),
            serde_json::Number::from_f64(temperature).map_or(Value::Null, Value::Number),
        );
    }
    result.insert("inferenceConfig".to_owned(), Value::Object(inference));
    if let Some(config) = convert_tool_config(
        context.tools.as_deref(),
        options.tool_choice.as_ref(),
        supports_strict_mode(model),
    )? {
        result.insert("toolConfig".to_owned(), config);
    }
    if let Some(fields) = build_additional_model_request_fields(model, options) {
        result.insert("additionalModelRequestFields".to_owned(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        result.insert(
            "requestMetadata".to_owned(),
            serde_json::to_value(metadata).map_err(BedrockError::display)?,
        );
    }
    Ok(Value::Object(result))
}

fn model_match_candidates(model_id: &str, model_name: &str) -> Vec<String> {
    [model_id, model_name]
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let mut normalized = String::with_capacity(lower.len());
            let mut separating = false;
            for character in lower.chars() {
                let separator = matches!(character, '_' | '.' | ':')
                    || trim_javascript_whitespace(character.encode_utf8(&mut [0; 4])).is_empty();
                if separator {
                    if !separating {
                        normalized.push('-');
                    }
                } else {
                    normalized.push(character);
                }
                separating = separator;
            }
            [lower, normalized]
        })
        .collect()
}

fn supports_adaptive_thinking(model_id: &str, model_name: &str) -> bool {
    model_match_candidates(model_id, model_name)
        .iter()
        .any(|candidate| {
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

fn supports_native_xhigh_effort(model: &Model) -> bool {
    model_match_candidates(&model.id, &model.name)
        .iter()
        .any(|candidate| {
            ["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "fable-5"]
                .iter()
                .any(|needle| candidate.contains(needle))
        })
}

fn thinking_level_mapping(model: &Model, level: ThinkingLevel) -> Option<&str> {
    let mapping = model.thinking_level_map.as_ref()?;
    let value = match level {
        ThinkingLevel::Minimal => mapping.minimal.as_ref(),
        ThinkingLevel::Low => mapping.low.as_ref(),
        ThinkingLevel::Medium => mapping.medium.as_ref(),
        ThinkingLevel::High => mapping.high.as_ref(),
        ThinkingLevel::Xhigh => mapping.xhigh.as_ref(),
        ThinkingLevel::Max => mapping.max.as_ref(),
    }?;
    value.as_deref()
}

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && supports_native_xhigh_effort(model) {
        return "xhigh".to_owned();
    }
    if let Some(mapped) = thinking_level_mapping(model, level) {
        return mapped.to_owned();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    }
    .to_owned()
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

fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = model_match_candidates(&model.id, &model.name);
    if !candidates
        .iter()
        .any(|candidate| candidate.contains("claude"))
    {
        return get_provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
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

fn cache_point(retention: CacheRetention) -> Value {
    if retention == CacheRetention::Long {
        json!({"cachePoint":{"type":"default","ttl":"1h"}})
    } else {
        json!({"cachePoint":{"type":"default"}})
    }
}

fn build_system_prompt(
    system_prompt: Option<&str>,
    model: &Model,
    retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Option<Vec<Value>> {
    let system_prompt = system_prompt.filter(|value| !value.is_empty())?;
    let mut blocks = vec![json!({"text":system_prompt})];
    if retention != CacheRetention::None && supports_prompt_caching(model, env) {
        blocks.push(cache_point(retention));
    }
    Some(blocks)
}

fn normalize_tool_call_id(id: &str, _model: &Model, _message: &AssistantMessage) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn non_blank_text(text: &str) -> Option<Value> {
    (!trim_javascript_whitespace(text).is_empty()).then(|| json!({"text":text}))
}

fn required_text(text: &str) -> Value {
    non_blank_text(text).unwrap_or_else(|| json!({"text":EMPTY_TEXT_PLACEHOLDER}))
}

fn sanitize_bedrock_document(value: &Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.iter().map(sanitize_bedrock_document).collect())
        }
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.clone(), sanitize_bedrock_document(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn image_block(mime_type: &str, data: &str) -> Result<Value, BedrockError> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => {
            return Err(BedrockError::plain(format!(
                "Unknown image type: {mime_type}"
            )));
        }
    };
    let bytes = decode_browser_base64(data)
        .map_err(|_| BedrockError::plain("The string to be decoded is not correctly encoded."))?;
    Ok(json!({"source":{"bytes":STANDARD.encode(bytes)},"format":format}))
}

fn decode_browser_base64(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let normalized = data
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\u{000c}' | '\r' | ' '))
        .collect::<String>();
    BROWSER_BASE64.decode(normalized)
}

fn convert_tool_result_content(content: &[UserContentBlock]) -> Result<Vec<Value>, BedrockError> {
    let mut result = Vec::new();
    for block in content {
        match block {
            UserContentBlock::Image(image) => {
                result.push(json!({"image":image_block(&image.mime_type, &image.data)?}));
            }
            UserContentBlock::Text(text) => {
                if let Some(block) = non_blank_text(&text.text) {
                    result.push(block);
                }
            }
            UserContentBlock::Unknown(_) => {}
        }
    }
    if result.is_empty() {
        result.push(json!({"text":EMPTY_TEXT_PLACEHOLDER}));
    }
    Ok(result)
}

fn convert_messages(
    context: &Context,
    model: &Model,
    retention: CacheRetention,
    env: Option<&ProviderEnv>,
) -> Result<Vec<Value>, BedrockError> {
    let transformed = transform_messages(&context.messages, model, Some(&normalize_tool_call_id));
    let mut result = Vec::new();
    let mut index = 0;
    while index < transformed.len() {
        match &transformed[index] {
            Message::User(message) => {
                let mut content = Vec::new();
                match &message.content {
                    UserContent::Text(text) => content.push(required_text(text)),
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                UserContentBlock::Text(text) => {
                                    if let Some(block) = non_blank_text(&text.text) {
                                        content.push(block);
                                    }
                                }
                                UserContentBlock::Image(image) => content.push(
                                    json!({"image":image_block(&image.mime_type, &image.data)?}),
                                ),
                                UserContentBlock::Unknown(_) => {}
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({"text":EMPTY_TEXT_PLACEHOLDER}));
                        }
                    }
                }
                result.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(message) => {
                if message.content.is_empty() {
                    index += 1;
                    continue;
                }
                let mut content = Vec::new();
                for block in message.content.iter() {
                    match block {
                        AssistantContent::Text(text) => {
                            if let Some(block) = non_blank_text(&text.text) {
                                content.push(block);
                            }
                        }
                        AssistantContent::ToolCall(call) => content.push(json!({
                            "toolUse":{
                                "toolUseId":call.id,
                                "name":call.name,
                                "input":sanitize_bedrock_document(&call.arguments)
                            }
                        })),
                        AssistantContent::Thinking(thinking) if thinking.redacted == Some(true) => {
                            if let Some(bytes) = thinking
                                .thinking_signature
                                .as_deref()
                                .and_then(|signature| decode_browser_base64(signature).ok())
                                .filter(|bytes| !bytes.is_empty())
                            {
                                content.push(json!({
                                    "reasoningContent":{"redactedContent":STANDARD.encode(bytes)}
                                }));
                            }
                        }
                        AssistantContent::Thinking(thinking) => {
                            if trim_javascript_whitespace(&thinking.thinking).is_empty() {
                                continue;
                            }
                            if is_anthropic_claude_model(model) {
                                if let Some(signature) =
                                    thinking.thinking_signature.as_deref().filter(|signature| {
                                        !trim_javascript_whitespace(signature).is_empty()
                                    })
                                {
                                    content.push(json!({
                                        "reasoningContent":{"reasoningText":{
                                            "text":thinking.thinking,
                                            "signature":signature
                                        }}
                                    }));
                                } else {
                                    content.push(json!({"text":thinking.thinking}));
                                }
                            } else {
                                content.push(json!({
                                    "reasoningContent":{"reasoningText":{"text":thinking.thinking}}
                                }));
                            }
                        }
                        AssistantContent::Unknown(_) => {}
                    }
                }
                if !content.is_empty() {
                    result.push(json!({"role":"assistant","content":content}));
                }
            }
            Message::ToolResult(_) => {
                let mut content = Vec::new();
                let mut cursor = index;
                while let Some(Message::ToolResult(message)) = transformed.get(cursor) {
                    content.push(json!({"toolResult":{
                        "toolUseId":message.tool_call_id,
                        "content":convert_tool_result_content(&message.content)?,
                        "status":if message.is_error { "error" } else { "success" }
                    }}));
                    cursor += 1;
                }
                result.push(json!({"role":"user","content":content}));
                index = cursor - 1;
            }
        }
        index += 1;
    }
    if retention != CacheRetention::None
        && supports_prompt_caching(model, env)
        && let Some(last) = result.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.push(cache_point(retention));
    }
    Ok(result)
}

fn convert_tool_config(
    tools: Option<&[Tool]>,
    choice: Option<&BedrockToolChoice>,
    supports_strict: bool,
) -> Result<Option<Value>, BedrockError> {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else {
        return Ok(None);
    };
    if matches!(
        choice,
        Some(BedrockToolChoice::Mode(BedrockToolChoiceMode::None))
    ) {
        return Ok(None);
    }
    let tools = tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict)
                .map_err(BedrockError::display)?;
            let mut spec = Map::from_iter([
                ("name".to_owned(), Value::String(tool.name.clone())),
                (
                    "description".to_owned(),
                    Value::String(tool.description.clone()),
                ),
                (
                    "inputSchema".to_owned(),
                    json!({"json":get_json_schema_tool_parameters(tool, strict)
                        .map_err(BedrockError::display)?}),
                ),
            ]);
            if strict == Some(true) {
                spec.insert("strict".to_owned(), Value::Bool(true));
            }
            Ok(json!({"toolSpec":spec}))
        })
        .collect::<Result<Vec<_>, BedrockError>>()?;
    let tool_choice = choice.map(|choice| match choice {
        BedrockToolChoice::Mode(BedrockToolChoiceMode::Auto) => json!({"auto":{}}),
        BedrockToolChoice::Mode(BedrockToolChoiceMode::Any) => json!({"any":{}}),
        BedrockToolChoice::Mode(BedrockToolChoiceMode::None) => Value::Null,
        BedrockToolChoice::Tool { name, .. } => json!({"tool":{"name":name}}),
    });
    let mut result = Map::from_iter([("tools".to_owned(), Value::Array(tools))]);
    if let Some(tool_choice) = tool_choice.filter(|choice| !choice.is_null()) {
        result.insert("toolChoice".to_owned(), tool_choice);
    }
    Ok(Some(Value::Object(result)))
}

fn is_govcloud_target(model: &Model, options: &BedrockOptions) -> bool {
    get_configured_region(options)
        .is_some_and(|region| region.to_lowercase().starts_with("us-gov-"))
        || model.id.to_lowercase().starts_with("us-gov.")
        || model.id.to_lowercase().starts_with("arn:aws-us-gov:")
}

fn budget_for_level(level: ThinkingLevel, budgets: Option<&ThinkingBudgets>) -> f64 {
    let clamped = clamp_reasoning(Some(level)).expect("level is present");
    let custom = budgets.and_then(|budgets| match clamped {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => budgets.high,
    });
    custom.unwrap_or(match level {
        ThinkingLevel::Minimal => 1_024.0,
        ThinkingLevel::Low => 2_048.0,
        ThinkingLevel::Medium => 8_192.0,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16_384.0,
    })
}

fn build_additional_model_request_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    let level = options.reasoning.filter(|_| model.reasoning)?;
    if !is_anthropic_claude_model(model) {
        return None;
    }
    let display = (!is_govcloud_target(model, options)).then(|| {
        options
            .thinking_display
            .unwrap_or(BedrockThinkingDisplay::Summarized)
    });
    if supports_adaptive_thinking(&model.id, &model.name) {
        let mut thinking =
            Map::from_iter([("type".to_owned(), Value::String("adaptive".to_owned()))]);
        if let Some(display) = display {
            thinking.insert(
                "display".to_owned(),
                serde_json::to_value(display).expect("display serializes"),
            );
        }
        return Some(json!({
            "thinking":thinking,
            "output_config":{"effort":map_thinking_level_to_effort(model, level)}
        }));
    }
    let mut thinking = Map::from_iter([
        ("type".to_owned(), Value::String("enabled".to_owned())),
        (
            "budget_tokens".to_owned(),
            crate::types::js_f64_value(budget_for_level(level, options.thinking_budgets.as_ref())),
        ),
    ]);
    if let Some(display) = display {
        thinking.insert(
            "display".to_owned(),
            serde_json::to_value(display).expect("display serializes"),
        );
    }
    let mut result = Map::from_iter([("thinking".to_owned(), Value::Object(thinking))]);
    if options.interleaved_thinking.unwrap_or(true) {
        result.insert(
            "anthropic_beta".to_owned(),
            json!(["interleaved-thinking-2025-05-14"]),
        );
    }
    Some(Value::Object(result))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaticCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientResolution {
    profile: Option<String>,
    region: Option<String>,
    endpoint: Option<String>,
    credentials: Option<StaticCredentials>,
    bearer_token: Option<String>,
    proxy_url: Option<Url>,
    force_http1: bool,
}

fn nonempty(value: Option<&String>) -> Option<String> {
    value.filter(|value| !value.is_empty()).cloned()
}

fn get_configured_region(options: &BedrockOptions) -> Option<String> {
    nonempty(options.region.as_ref())
        .or_else(|| get_provider_env_value("AWS_REGION", options.stream.request.env.as_ref()))
        .or_else(|| {
            get_provider_env_value("AWS_DEFAULT_REGION", options.stream.request.env.as_ref())
        })
}

fn get_configured_credentials(env: Option<&ProviderEnv>) -> Option<StaticCredentials> {
    let access_key_id = get_provider_env_value("AWS_ACCESS_KEY_ID", env)?;
    let secret_access_key = get_provider_env_value("AWS_SECRET_ACCESS_KEY", env)?;
    Some(StaticCredentials {
        access_key_id,
        secret_access_key,
        session_token: get_provider_env_value("AWS_SESSION_TOKEN", env),
    })
}

fn standard_endpoint_region(base_url: &str) -> Option<String> {
    let hostname = Url::parse(base_url).ok()?.host_str()?.to_lowercase();
    let rest = hostname
        .strip_prefix("bedrock-runtime.")
        .or_else(|| hostname.strip_prefix("bedrock-runtime-fips."))?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    (!region.is_empty()
        && region.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }))
    .then(|| region.to_owned())
}

fn should_use_explicit_endpoint(
    base_url: &str,
    configured_region: Option<&str>,
    has_ambient_profile: bool,
) -> bool {
    standard_endpoint_region(base_url).is_none()
        || (configured_region.is_none() && !has_ambient_profile)
}

fn arn_region(model_id: &str) -> Option<String> {
    let mut parts = model_id.split(':');
    let arn = parts.next()?;
    let partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    let valid_partition = partition == "aws"
        || partition
            .strip_prefix("aws-")
            .is_some_and(|suffix| !suffix.is_empty());
    (arn == "arn"
        && valid_partition
        && partition.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && service == "bedrock"
        && !region.is_empty()
        && region.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }))
    .then(|| region.to_owned())
}

fn resolve_client_configuration(
    model: &Model,
    options: &BedrockOptions,
) -> Result<ClientResolution, BedrockError> {
    let env = options.stream.request.env.as_ref();
    let options_profile = nonempty(options.profile.as_ref())
        .or_else(|| env.and_then(|env| nonempty(env.get("AWS_PROFILE"))));
    let profile = options_profile
        .clone()
        .or_else(|| get_provider_env_value("AWS_PROFILE", env));
    let configured_region = get_configured_region(options);
    let has_ambient_profile = get_provider_env_value("AWS_PROFILE", None).is_some();
    let endpoint_region = standard_endpoint_region(&model.base_url);
    let explicit_endpoint = should_use_explicit_endpoint(
        &model.base_url,
        configured_region.as_deref(),
        has_ambient_profile,
    );
    let region = arn_region(&model.id)
        .or(configured_region)
        .or_else(|| explicit_endpoint.then_some(endpoint_region).flatten())
        .or_else(|| (!has_ambient_profile).then(|| "us-east-1".to_owned()));
    let skip_auth = get_provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    let bearer_token = nonempty(options.bearer_token.as_ref())
        .or_else(|| nonempty(options.stream.request.api_key.as_ref()))
        .or_else(|| get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env))
        .filter(|_| !skip_auth);
    let credentials = if skip_auth {
        Some(StaticCredentials {
            access_key_id: "dummy-access-key".to_owned(),
            secret_access_key: "dummy-secret-key".to_owned(),
            session_token: None,
        })
    } else if options_profile.is_none() {
        get_configured_credentials(env)
    } else {
        None
    };
    let proxy_url =
        resolve_http_proxy_url_for_target(&model.base_url, env).map_err(BedrockError::display)?;
    Ok(ClientResolution {
        profile,
        region,
        endpoint: explicit_endpoint.then(|| model.base_url.clone()),
        credentials,
        bearer_token,
        proxy_url,
        force_http1: get_provider_env_value("AWS_BEDROCK_FORCE_HTTP1", env).as_deref() == Some("1"),
    })
}

#[derive(Debug, Clone)]
struct BedrockError {
    message: String,
    details: Box<BedrockErrorDetails>,
}

#[derive(Debug, Clone, Default)]
struct BedrockErrorDetails {
    name: Option<String>,
    status: Option<i64>,
    body: Option<String>,
    message_carries_body: bool,
    service_exception: bool,
    request_id: Option<String>,
    fallback_request_id: Option<String>,
}

impl std::ops::Deref for BedrockError {
    type Target = BedrockErrorDetails;

    fn deref(&self) -> &Self::Target {
        &self.details
    }
}

impl BedrockError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: Box::new(BedrockErrorDetails {
                message_carries_body: true,
                ..BedrockErrorDetails::default()
            }),
        }
    }

    fn display(error: impl fmt::Display) -> Self {
        Self::plain(error.to_string())
    }

    fn with_fallback_request_id(mut self, request_id: Option<String>) -> Self {
        self.details.fallback_request_id = request_id;
        self
    }
}

impl fmt::Display for BedrockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BedrockError {}

fn format_bedrock_error(error: &BedrockError) -> String {
    let core = if !error.message_carries_body {
        match (error.status, error.body.as_deref()) {
            (Some(status), Some(body)) => format!("{status}: {body}"),
            _ => error.message.clone(),
        }
    } else {
        error.message.clone()
    };
    let hint = if core.to_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    if !error.service_exception {
        return format!("{core}{hint}");
    }
    let name = error
        .name
        .as_deref()
        .unwrap_or("BedrockRuntimeServiceException");
    let prefix = match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        value => value,
    };
    format!("{prefix}: {core}{hint}")
}

fn normalize_diagnostic_value(value: Option<&str>) -> Option<String> {
    let value = trim_javascript_whitespace(value?);
    (!value.is_empty() && value.encode_utf16().count() <= MAX_BEDROCK_DIAGNOSTIC_VALUE_CHARS)
        .then(|| value.to_owned())
}

fn append_bedrock_failure_diagnostic(
    output: &mut AssistantMessage,
    error: &BedrockError,
    fallback_request_id: Option<&str>,
) {
    let mut details = Map::new();
    if let Some(status) = error.status {
        details.insert("status".to_owned(), Value::from(status));
    }
    if let Some(code) = error
        .name
        .as_deref()
        .filter(|name| name.ends_with("Exception"))
        .and_then(|name| normalize_diagnostic_value(Some(name)))
    {
        details.insert("errorCode".to_owned(), Value::String(code));
    }
    if let Some(request_id) = normalize_diagnostic_value(error.request_id.as_deref())
        .or_else(|| normalize_diagnostic_value(fallback_request_id))
    {
        details.insert("requestId".to_owned(), Value::String(request_id));
    }
    if details.is_empty() {
        return;
    }
    output
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(AssistantMessageDiagnostic {
            kind: "bedrock_response_failure".to_owned(),
            timestamp: now_millis(),
            error: None,
            details: Some(details),
        });
}

fn is_reserved_header(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "authorization" || key == "host" || key.starts_with("x-amz-")
}

#[derive(Debug, Clone)]
enum BedrockStreamEvent {
    MessageStart {
        role: String,
    },
    ContentBlockStart {
        provider_index: i32,
        tool_id: Option<String>,
        tool_name: Option<String>,
    },
    TextDelta {
        provider_index: i32,
        text: String,
    },
    ToolDelta {
        provider_index: i32,
        input: String,
    },
    ReasoningDelta {
        provider_index: i32,
        text: Option<String>,
        signature: Option<String>,
        redacted_content: Option<Vec<u8>>,
    },
    ContentBlockStop {
        provider_index: i32,
    },
    MessageStop {
        reason: Option<String>,
    },
    Metadata {
        input: Option<i64>,
        output: Option<i64>,
        cache_read: Option<i64>,
        cache_write: Option<i64>,
        total: Option<i64>,
    },
}

#[derive(Debug, Clone)]
struct StreamingBlockState {
    content_index: usize,
    partial_json: String,
    redacted_chunks: Vec<Vec<u8>>,
}

#[derive(Debug, Default)]
struct StreamState {
    blocks: BTreeMap<i32, StreamingBlockState>,
}

fn handle_stream_event(
    event: BedrockStreamEvent,
    state: &mut StreamState,
    model: &Model,
    output: &mut AssistantMessage,
    sender: &AssistantStreamSender,
) -> Result<(), BedrockError> {
    match event {
        BedrockStreamEvent::MessageStart { role } => {
            if role != "assistant" {
                return Err(BedrockError::plain(
                    "Unexpected assistant message start but got user message start instead",
                ));
            }
            sender
                .send(AssistantMessageEvent::Start)
                .map_err(|_| BedrockError::plain("Assistant event stream receiver was dropped"))?;
        }
        BedrockStreamEvent::ContentBlockStart {
            provider_index,
            tool_id,
            tool_name,
        } => {
            if tool_id.is_some() || tool_name.is_some() {
                let content_index = output.content.len();
                let call = ToolCall::new(
                    tool_id.unwrap_or_default(),
                    tool_name.unwrap_or_default(),
                    Value::Object(Map::new()),
                );
                output
                    .content
                    .push(AssistantContent::ToolCall(call.clone()));
                state.blocks.insert(
                    provider_index,
                    StreamingBlockState {
                        content_index,
                        partial_json: String::new(),
                        redacted_chunks: Vec::new(),
                    },
                );
                sender
                    .send(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        id: call.id,
                        tool_name: call.name,
                        namespace: None,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
            }
        }
        BedrockStreamEvent::TextDelta {
            provider_index,
            text,
        } => {
            let content_index = if let Some(block) = state.blocks.get(&provider_index) {
                block.content_index
            } else {
                let content_index = output.content.len();
                output
                    .content
                    .push(AssistantContent::Text(TextContent::new("")));
                state.blocks.insert(
                    provider_index,
                    StreamingBlockState {
                        content_index,
                        partial_json: String::new(),
                        redacted_chunks: Vec::new(),
                    },
                );
                sender
                    .send(AssistantMessageEvent::TextStart { content_index })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
                content_index
            };
            if let Some(AssistantContent::Text(block)) = output.content.get_mut(content_index) {
                block.text.push_str(&text);
                sender
                    .send(AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: text,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
            }
        }
        BedrockStreamEvent::ToolDelta {
            provider_index,
            input,
        } => {
            if let Some(block) = state.blocks.get_mut(&provider_index)
                && let Some(AssistantContent::ToolCall(call)) =
                    output.content.get_mut(block.content_index)
            {
                block.partial_json.push_str(&input);
                call.arguments = parse_streaming_json(Some(&block.partial_json));
                sender
                    .send(AssistantMessageEvent::ToolCallDelta {
                        content_index: block.content_index,
                        delta: input,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
            }
        }
        BedrockStreamEvent::ReasoningDelta {
            provider_index,
            text,
            signature,
            redacted_content,
        } => {
            let content_index = if let Some(block) = state.blocks.get(&provider_index) {
                block.content_index
            } else {
                let content_index = output.content.len();
                let mut thinking = ThinkingContent::new("");
                thinking.thinking_signature = Some(String::new());
                output.content.push(AssistantContent::Thinking(thinking));
                state.blocks.insert(
                    provider_index,
                    StreamingBlockState {
                        content_index,
                        partial_json: String::new(),
                        redacted_chunks: Vec::new(),
                    },
                );
                sender
                    .send(AssistantMessageEvent::ThinkingStart {
                        content_index,
                        thinking: None,
                        thinking_signature: None,
                        redacted: None,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
                content_index
            };
            let Some(AssistantContent::Thinking(thinking)) = output.content.get_mut(content_index)
            else {
                return Ok(());
            };
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                thinking.thinking.push_str(&text);
                sender
                    .send(AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: text,
                        thinking_signature_delta: None,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?;
            }
            if thinking.redacted != Some(true)
                && let Some(signature) = signature.filter(|signature| !signature.is_empty())
            {
                thinking
                    .thinking_signature
                    .get_or_insert_with(String::new)
                    .push_str(&signature);
            }
            if let Some(redacted_content) = redacted_content.filter(|content| !content.is_empty()) {
                if thinking.redacted != Some(true) {
                    thinking.redacted = Some(true);
                    thinking.thinking_signature = Some(String::new());
                    thinking.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                    sender
                        .send(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: REDACTED_THINKING_PLACEHOLDER.to_owned(),
                            thinking_signature_delta: None,
                        })
                        .map_err(|_| {
                            BedrockError::plain("Assistant event stream receiver was dropped")
                        })?;
                }
                if let Some(block) = state.blocks.get_mut(&provider_index) {
                    block.redacted_chunks.push(redacted_content);
                }
            }
        }
        BedrockStreamEvent::ContentBlockStop { provider_index } => {
            let Some(block) = state.blocks.remove(&provider_index) else {
                return Ok(());
            };
            let Some(content) = output.content.get_mut(block.content_index) else {
                return Ok(());
            };
            match content {
                AssistantContent::Text(text) => sender
                    .send(AssistantMessageEvent::TextEnd {
                        content_index: block.content_index,
                        content: text.text.clone(),
                        content_signature: None,
                    })
                    .map_err(|_| {
                        BedrockError::plain("Assistant event stream receiver was dropped")
                    })?,
                AssistantContent::Thinking(thinking) => {
                    flush_redacted_content(thinking, &block.redacted_chunks);
                    sender
                        .send(AssistantMessageEvent::ThinkingEnd {
                            content_index: block.content_index,
                            content: thinking.thinking.clone(),
                            content_signature: None,
                            redacted: None,
                        })
                        .map_err(|_| {
                            BedrockError::plain("Assistant event stream receiver was dropped")
                        })?;
                }
                AssistantContent::ToolCall(call) => {
                    call.arguments = parse_streaming_json(Some(&block.partial_json));
                    sender
                        .send(AssistantMessageEvent::ToolCallEnd {
                            content_index: block.content_index,
                            tool_call: call.clone(),
                        })
                        .map_err(|_| {
                            BedrockError::plain("Assistant event stream receiver was dropped")
                        })?;
                }
                AssistantContent::Unknown(_) => {}
            }
        }
        BedrockStreamEvent::MessageStop { reason } => {
            output.raw_stop_reason.clone_from(&reason);
            let (stop_reason, message) = map_stop_reason(reason.as_deref());
            output.stop_reason = stop_reason;
            if message.is_some() {
                output.error_message = message;
            }
        }
        BedrockStreamEvent::Metadata {
            input,
            output: output_tokens,
            cache_read,
            cache_write,
            total,
        } => {
            if input.is_none()
                && output_tokens.is_none()
                && cache_read.is_none()
                && cache_write.is_none()
                && total.is_none()
            {
                return Ok(());
            }
            let input = input.unwrap_or(0);
            let output_tokens = output_tokens.unwrap_or(0);
            output.usage.input = UsageValue::from(input);
            output.usage.output = UsageValue::from(output_tokens);
            output.usage.cache_read = UsageValue::from(cache_read.unwrap_or(0));
            output.usage.cache_write = UsageValue::from(cache_write.unwrap_or(0));
            output.usage.total_tokens = UsageValue::from(
                total
                    .filter(|total| *total != 0)
                    .unwrap_or(input + output_tokens),
            );
            calculate_cost(model, &mut output.usage);
        }
    }
    Ok(())
}

fn flush_redacted_content(thinking: &mut ThinkingContent, chunks: &[Vec<u8>]) {
    if !chunks.is_empty() {
        thinking.thinking_signature = Some(STANDARD.encode(chunks.concat()));
    }
}

fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(reason) => (
            StopReason::Error,
            Some(format!("Provider stopped with: {reason}")),
        ),
        None => (StopReason::Error, None),
    }
}

mod sdk {
    use super::*;
    use aws_credential_types::{Credentials, Token};
    use aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError;
    use aws_sdk_bedrockruntime::types::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseStreamOutput,
        Message, ReasoningContentBlockDelta,
    };
    use aws_smithy_runtime_api::box_error::BoxError;
    use aws_smithy_runtime_api::client::auth::AuthSchemeId;
    use aws_smithy_runtime_api::client::http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    };
    use aws_smithy_runtime_api::client::interceptors::Intercept;
    use aws_smithy_runtime_api::client::interceptors::context::{
        BeforeDeserializationInterceptorContextRef, BeforeTransmitInterceptorContextMut,
    };
    use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
    use aws_smithy_runtime_api::client::result::{ConnectorError, SdkError};
    use aws_smithy_runtime_api::client::runtime_components::{
        RuntimeComponents, RuntimeComponentsBuilder,
    };
    use aws_smithy_types::body::SdkBody;
    use aws_smithy_types::config_bag::ConfigBag;
    use aws_smithy_types::error::metadata::ProvideErrorMetadata;
    use aws_smithy_types::event_stream::RawMessage;
    use aws_types::region::Region;
    use aws_types::request_id::RequestId;
    use bytes::Bytes;
    use http_body::{Body, Frame};
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    type ByteStream = Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

    struct ResponseBody {
        stream: Mutex<ByteStream>,
    }

    impl fmt::Debug for ResponseBody {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("ResponseBody")
        }
    }

    impl Body for ResponseBody {
        type Data = Bytes;
        type Error = reqwest::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
            stream
                .as_mut()
                .poll_next(context)
                .map(|item| item.map(|result| result.map(Frame::data)))
        }
    }

    #[derive(Clone)]
    struct ReqwestConnector {
        client: Option<reqwest::Client>,
        build_error: Option<String>,
    }

    impl fmt::Debug for ReqwestConnector {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("ReqwestConnector")
                .field("ready", &self.client.is_some())
                .finish()
        }
    }

    impl HttpConnector for ReqwestConnector {
        fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
            let Some(client) = self.client.clone() else {
                return HttpConnectorFuture::ready(Err(ConnectorError::user(
                    std::io::Error::other(
                        self.build_error
                            .clone()
                            .unwrap_or_else(|| "failed to build HTTP client".to_owned()),
                    )
                    .into(),
                )));
            };
            let body = match request.body().bytes() {
                Some(body) => body.to_vec(),
                None => {
                    return HttpConnectorFuture::ready(Err(ConnectorError::user(
                        std::io::Error::other("AWS request body is not replayable").into(),
                    )));
                }
            };
            let request = match request.try_into_http1x() {
                Ok(request) => request,
                Err(error) => {
                    return HttpConnectorFuture::ready(Err(ConnectorError::user(error.into())));
                }
            };
            let (parts, _) = request.into_parts();
            HttpConnectorFuture::new(async move {
                let response = client
                    .request(parts.method, parts.uri.to_string())
                    .headers(parts.headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(connector_error)?;
                let status = response.status();
                let headers = response.headers().clone();
                let body = ResponseBody {
                    stream: Mutex::new(Box::pin(response.bytes_stream())),
                };
                let response = http::Response::builder()
                    .status(status)
                    .body(SdkBody::from_body_1_x(body))
                    .map_err(|error| ConnectorError::user(error.into()))?;
                let mut response = response;
                *response.headers_mut() = headers;
                HttpResponse::try_from(response)
                    .map_err(|error| ConnectorError::other(error.into(), None))
            })
        }
    }

    fn connector_error(error: reqwest::Error) -> ConnectorError {
        if error.is_timeout() {
            ConnectorError::timeout(error.into())
        } else if error.is_connect() {
            ConnectorError::io(error.into()).never_connected()
        } else {
            ConnectorError::other(error.into(), None)
        }
    }

    #[derive(Clone)]
    struct ReqwestHttpClient {
        proxy_url: Option<Url>,
        force_http1: bool,
    }

    impl fmt::Debug for ReqwestHttpClient {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("ReqwestHttpClient")
                .field("proxy_url", &self.proxy_url)
                .field("force_http1", &self.force_http1)
                .finish()
        }
    }

    impl HttpClient for ReqwestHttpClient {
        fn http_connector(
            &self,
            settings: &HttpConnectorSettings,
            _components: &RuntimeComponents,
        ) -> SharedHttpConnector {
            let mut builder = reqwest::Client::builder();
            if let Some(timeout) = settings.connect_timeout() {
                builder = builder.connect_timeout(timeout);
            }
            if let Some(timeout) = settings.read_timeout() {
                builder = builder.read_timeout(timeout);
            }
            if self.force_http1 || self.proxy_url.is_some() {
                builder = builder.http1_only();
            }
            let build_result = self
                .proxy_url
                .as_ref()
                .map(|url| reqwest::Proxy::all(url.as_str()))
                .transpose()
                .and_then(|proxy| {
                    if let Some(proxy) = proxy {
                        builder = builder.proxy(proxy);
                    }
                    builder.build()
                });
            let (client, build_error) = match build_result {
                Ok(client) => (Some(client), None),
                Err(error) => (None, Some(error.to_string())),
            };
            SharedHttpConnector::new(ReqwestConnector {
                client,
                build_error,
            })
        }

        fn validate_base_client_config(
            &self,
            _runtime_components: &RuntimeComponentsBuilder,
            _cfg: &ConfigBag,
        ) -> Result<(), BoxError> {
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    struct BedrockInterceptor {
        body: Arc<Vec<u8>>,
        headers: IndexMap<String, String>,
        response: Arc<Mutex<Option<ProviderResponse>>>,
    }

    impl Intercept for BedrockInterceptor {
        fn name(&self) -> &'static str {
            "PiAiBedrockInterceptor"
        }

        fn modify_before_signing(
            &self,
            context: &mut BeforeTransmitInterceptorContextMut<'_>,
            _runtime_components: &RuntimeComponents,
            _cfg: &mut ConfigBag,
        ) -> Result<(), BoxError> {
            let request = context.request_mut();
            *request.body_mut() = SdkBody::from(self.body.as_ref().clone());
            request
                .headers_mut()
                .try_insert("content-length", self.body.len().to_string())?;
            apply_custom_headers(request.headers_mut(), &self.headers)?;
            Ok(())
        }

        fn read_before_deserialization(
            &self,
            context: &BeforeDeserializationInterceptorContextRef<'_>,
            _runtime_components: &RuntimeComponents,
            _cfg: &mut ConfigBag,
        ) -> Result<(), BoxError> {
            let response = context.response();
            *self.response.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(to_provider_response(response));
            Ok(())
        }
    }

    pub(super) fn apply_custom_headers(
        headers: &mut aws_smithy_runtime_api::http::Headers,
        custom: &IndexMap<String, String>,
    ) -> Result<(), BoxError> {
        for (key, value) in custom {
            if !is_reserved_header(key) {
                headers.try_insert(key.clone(), value.clone())?;
            }
        }
        Ok(())
    }

    pub(super) fn to_provider_response(response: &HttpResponse) -> ProviderResponse {
        ProviderResponse {
            status: response.status().as_u16(),
            headers: response
                .headers()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        }
    }

    fn request_id_from_headers(headers: &BTreeMap<String, String>) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| {
                matches!(
                    key.to_ascii_lowercase().as_str(),
                    "x-amzn-requestid" | "x-amz-request-id"
                )
            })
            .and_then(|(_, value)| normalize_diagnostic_value(Some(value)))
    }

    fn sdk_error<E>(error: &SdkError<E, HttpResponse>) -> BedrockError
    where
        E: std::error::Error + ProvideErrorMetadata + 'static,
    {
        let raw = error.raw_response();
        let status = raw.map(|response| i64::from(response.status().as_u16()));
        let body = raw
            .and_then(|response| response.body().bytes())
            .and_then(|body| std::str::from_utf8(body).ok())
            .map(str::to_owned);
        let service = error.as_service_error();
        let name = service
            .and_then(ProvideErrorMetadata::code)
            .map(str::to_owned);
        let message = service
            .and_then(ProvideErrorMetadata::message)
            .map(str::to_owned)
            .unwrap_or_else(|| most_specific_error_message(error));
        let request_id = raw.and_then(|response| {
            response
                .headers()
                .into_iter()
                .find(|(key, _)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "x-amzn-requestid" | "x-amz-request-id"
                    )
                })
                .map(|(_, value)| value.to_owned())
        });
        let message_carries_body = body.as_ref().is_none_or(|body| message.contains(body));
        BedrockError {
            message,
            details: Box::new(BedrockErrorDetails {
                message_carries_body,
                name,
                status,
                body,
                service_exception: service.is_some(),
                request_id,
                fallback_request_id: None,
            }),
        }
    }

    pub(super) fn event_stream_error(
        error: &SdkError<ConverseStreamOutputError, RawMessage>,
        fallback_request_id: Option<String>,
    ) -> BedrockError {
        if let Some(service) = error.as_service_error() {
            let modeled = match service {
                ConverseStreamOutputError::InternalServerException(error) => {
                    Some(("InternalServerException", error.message()))
                }
                ConverseStreamOutputError::ModelStreamErrorException(error) => {
                    Some(("ModelStreamErrorException", error.message()))
                }
                ConverseStreamOutputError::ValidationException(error) => {
                    Some(("ValidationException", error.message()))
                }
                ConverseStreamOutputError::ThrottlingException(error) => {
                    Some(("ThrottlingException", error.message()))
                }
                ConverseStreamOutputError::ServiceUnavailableException(error) => {
                    Some(("ServiceUnavailableException", error.message()))
                }
                _ => None,
            };
            if let Some((name, message)) = modeled {
                // pi `bedrock-converse-stream.ts:264-278,385,409-410` receives these
                // event members as bare objects, not Bedrock service exceptions or Errors.
                return bare_modeled_stream_exception(name, message, fallback_request_id);
            }
        }
        let service = error.as_service_error();
        let name = service
            .and_then(ProvideErrorMetadata::code)
            .map(str::to_owned);
        BedrockError {
            message: service
                .and_then(ProvideErrorMetadata::message)
                .map(str::to_owned)
                .unwrap_or_else(|| most_specific_error_message(error)),
            details: Box::new(BedrockErrorDetails {
                service_exception: false,
                name,
                message_carries_body: true,
                request_id: service.and_then(RequestId::request_id).map(str::to_owned),
                fallback_request_id,
                ..BedrockErrorDetails::default()
            }),
        }
    }

    fn most_specific_error_message(error: &(dyn std::error::Error + 'static)) -> String {
        let mut message = error.to_string();
        let mut current = error;
        while let Some(source) = current.source() {
            let next = source.to_string();
            if !next.is_empty() {
                message = next;
            }
            current = source;
        }
        message
    }

    fn bare_modeled_stream_exception(
        name: &str,
        message: Option<&str>,
        fallback_request_id: Option<String>,
    ) -> BedrockError {
        BedrockError::plain(message.unwrap_or(name)).with_fallback_request_id(fallback_request_id)
    }

    pub(super) fn convert_sdk_event(event: ConverseStreamOutput) -> Option<BedrockStreamEvent> {
        match event {
            ConverseStreamOutput::MessageStart(event) => Some(BedrockStreamEvent::MessageStart {
                role: event.role().as_str().to_owned(),
            }),
            ConverseStreamOutput::ContentBlockStart(event) => {
                let (tool_id, tool_name) = match event.start() {
                    Some(ContentBlockStart::ToolUse(tool)) => (
                        Some(tool.tool_use_id().to_owned()),
                        Some(tool.name().to_owned()),
                    ),
                    _ => (None, None),
                };
                Some(BedrockStreamEvent::ContentBlockStart {
                    provider_index: event.content_block_index(),
                    tool_id,
                    tool_name,
                })
            }
            ConverseStreamOutput::ContentBlockDelta(event) => match event.delta()? {
                ContentBlockDelta::Text(text) => Some(BedrockStreamEvent::TextDelta {
                    provider_index: event.content_block_index(),
                    text: text.clone(),
                }),
                ContentBlockDelta::ToolUse(tool) => Some(BedrockStreamEvent::ToolDelta {
                    provider_index: event.content_block_index(),
                    input: tool.input().to_owned(),
                }),
                ContentBlockDelta::ReasoningContent(reasoning) => {
                    let (text, signature, redacted_content) = match reasoning {
                        ReasoningContentBlockDelta::Text(text) => (Some(text.clone()), None, None),
                        ReasoningContentBlockDelta::Signature(signature) => {
                            (None, Some(signature.clone()), None)
                        }
                        ReasoningContentBlockDelta::RedactedContent(content) => {
                            (None, None, Some(content.as_ref().to_vec()))
                        }
                        _ => return None,
                    };
                    Some(BedrockStreamEvent::ReasoningDelta {
                        provider_index: event.content_block_index(),
                        text,
                        signature,
                        redacted_content,
                    })
                }
                _ => None,
            },
            ConverseStreamOutput::ContentBlockStop(event) => {
                Some(BedrockStreamEvent::ContentBlockStop {
                    provider_index: event.content_block_index(),
                })
            }
            ConverseStreamOutput::MessageStop(event) => Some(BedrockStreamEvent::MessageStop {
                reason: Some(event.stop_reason().as_str().to_owned()),
            }),
            ConverseStreamOutput::Metadata(event) => {
                let usage = event.usage()?;
                Some(BedrockStreamEvent::Metadata {
                    input: Some(i64::from(usage.input_tokens())),
                    output: Some(i64::from(usage.output_tokens())),
                    cache_read: usage.cache_read_input_tokens().map(i64::from),
                    cache_write: usage.cache_write_input_tokens().map(i64::from),
                    total: Some(i64::from(usage.total_tokens())),
                })
            }
            _ => None,
        }
    }

    async fn await_with_abort<F, T>(options: &BedrockOptions, future: F) -> Result<T, BedrockError>
    where
        F: std::future::Future<Output = T>,
    {
        if let Some(signal) = &options.stream.request.signal {
            tokio::pin!(future);
            tokio::select! {
                biased;
                _ = signal.cancelled() => Err(BedrockError::plain("Request was aborted")),
                result = &mut future => Ok(result),
            }
        } else {
            Ok(future.await)
        }
    }

    fn finish_unstopped_blocks(state: &mut StreamState, output: &mut AssistantMessage) {
        for block in std::mem::take(&mut state.blocks).into_values() {
            match output.content.get_mut(block.content_index) {
                Some(AssistantContent::Thinking(thinking)) => {
                    flush_redacted_content(thinking, &block.redacted_chunks);
                }
                Some(AssistantContent::ToolCall(call)) => {
                    call.arguments = parse_streaming_json(Some(&block.partial_json));
                }
                Some(AssistantContent::Text(_)) | None => {}
                Some(AssistantContent::Unknown(_)) => {}
            }
        }
    }

    pub(super) fn finish_stream_result(
        state: &mut StreamState,
        output: &mut AssistantMessage,
        result: Result<(), BedrockError>,
    ) -> Result<(), BedrockError> {
        finish_unstopped_blocks(state, output);
        result
    }

    pub(super) async fn run(
        sender: &AssistantStreamSender,
        model: &Model,
        options: &BedrockOptions,
        output: &mut AssistantMessage,
        resolution: ClientResolution,
        mut payload: Value,
    ) -> Result<Option<String>, BedrockError> {
        let model_id = payload
            .get("modelId")
            .and_then(Value::as_str)
            .filter(|model_id| !model_id.is_empty())
            .ok_or_else(|| BedrockError::plain("No value provided for input HTTP label: modelId"))?
            .to_owned();
        payload
            .as_object_mut()
            .expect("command input is an object")
            .remove("modelId");
        let body = serde_json::to_vec(&payload).map_err(BedrockError::display)?;
        let headers =
            provider_headers_to_record(options.stream.request.headers.as_ref()).unwrap_or_default();
        let response_capture = Arc::new(Mutex::new(None));
        let interceptor = BedrockInterceptor {
            body: Arc::new(body),
            headers,
            response: Arc::clone(&response_capture),
        };

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(profile) = resolution.profile {
            loader = loader.profile_name(profile);
        }
        if let Some(region) = resolution.region {
            loader = loader.region(Region::new(region));
        }
        if let Some(credentials) = resolution.credentials {
            loader = loader.credentials_provider(Credentials::new(
                credentials.access_key_id,
                credentials.secret_access_key,
                credentials.session_token,
                None,
                "pi-ai",
            ));
        }
        if let Some(token) = resolution.bearer_token {
            loader = loader
                .token_provider(Token::new(token, None))
                .auth_scheme_preference([AuthSchemeId::from("httpBearerAuth")]);
        }
        if resolution.proxy_url.is_some() || resolution.force_http1 {
            loader = loader.http_client(ReqwestHttpClient {
                proxy_url: resolution.proxy_url,
                force_http1: resolution.force_http1,
            });
        }
        let shared_config = await_with_abort(options, loader.load()).await?;
        let mut config =
            aws_sdk_bedrockruntime::config::Builder::from(&shared_config).interceptor(interceptor);
        if let Some(endpoint) = resolution.endpoint {
            config = config.endpoint_url(endpoint);
        }
        let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
        let placeholder = Message::builder()
            .role(ConversationRole::User)
            .content(ContentBlock::Text(EMPTY_TEXT_PLACEHOLDER.to_owned()))
            .build()
            .map_err(BedrockError::display)?;
        let response = await_with_abort(
            options,
            client
                .converse_stream()
                .model_id(model_id)
                .messages(placeholder)
                .send(),
        )
        .await?
        .map_err(|error| sdk_error(&error))?;

        let modeled_request_id = normalize_diagnostic_value(response.request_id());

        let raw_response = response_capture
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let response_request_id = raw_response
            .as_ref()
            .and_then(|response| request_id_from_headers(&response.headers))
            .or(modeled_request_id);
        if let Some(on_response) = &options.stream.request.on_response {
            let response = raw_response.unwrap_or_else(|| ProviderResponse {
                status: 200,
                headers: response_request_id
                    .as_ref()
                    .map(|request_id| {
                        BTreeMap::from([("x-amzn-requestid".to_owned(), request_id.clone())])
                    })
                    .unwrap_or_default(),
            });
            on_response(response, model).await.map_err(|error| {
                BedrockError::plain(error).with_fallback_request_id(response_request_id.clone())
            })?;
        }

        let mut stream = response.stream;
        let mut state = StreamState::default();
        let receive_result = async {
            loop {
                let next = await_with_abort(options, stream.recv()).await?;
                let Some(event) =
                    next.map_err(|error| event_stream_error(&error, response_request_id.clone()))?
                else {
                    break;
                };
                let Some(event) = convert_sdk_event(event) else {
                    continue;
                };
                handle_stream_event(event, &mut state, model, output, sender)
                    .map_err(|error| error.with_fallback_request_id(response_request_id.clone()))?;
            }
            Ok::<(), BedrockError>(())
        }
        .await;
        finish_stream_result(&mut state, output, receive_result)?;
        Ok(response_request_id)
    }
}

#[cfg(test)]
mod tests;
