use crate::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::api::transform_messages::transform_messages;
use crate::types::{
    AssistantContent, Context, Message, Model, ModelInput, ModelThinkingLevel, StopReason, Tool,
    UserContent, UserContentBlock,
};
use crate::utils::provider_retry::{
    ProviderErrorMetadata, ProviderRetryClassify, ProviderRetryError, ProviderRetryOptions,
    retry_provider_request,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GoogleApiThinkingLevel {
    ThinkingLevelUnspecified,
    Minimal,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGoogleThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedGoogleThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

pub fn resolve_google_thinking_level(
    model: &Model,
    level: ModelThinkingLevel,
) -> Result<ResolvedGoogleThinkingLevel, String> {
    if level == ModelThinkingLevel::Off {
        return Ok(ResolvedGoogleThinkingLevel::High);
    }
    let mapped = model.thinking_level_map.as_ref().and_then(|mapping| {
        let entry = match level {
            ModelThinkingLevel::Off => &mapping.off,
            ModelThinkingLevel::Minimal => &mapping.minimal,
            ModelThinkingLevel::Low => &mapping.low,
            ModelThinkingLevel::Medium => &mapping.medium,
            ModelThinkingLevel::High => &mapping.high,
            ModelThinkingLevel::Xhigh => &mapping.xhigh,
            ModelThinkingLevel::Max => &mapping.max,
        };
        entry.as_ref().and_then(Option::as_deref)
    });
    let source = mapped.unwrap_or(match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    });
    match source.to_ascii_lowercase().as_str() {
        "minimal" => Ok(ResolvedGoogleThinkingLevel::Minimal),
        "low" => Ok(ResolvedGoogleThinkingLevel::Low),
        "medium" => Ok(ResolvedGoogleThinkingLevel::Medium),
        "high" => Ok(ResolvedGoogleThinkingLevel::High),
        _ => Err(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
            model.provider,
            model.id,
            level_name(level),
            mapped.unwrap_or("undefined")
        )),
    }
}

fn level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

pub fn is_thinking_part(part: &Value) -> bool {
    part.get("thought").and_then(Value::as_bool) == Some(true)
}

pub fn retain_thought_signature(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    incoming
        .filter(|signature| !signature.is_empty())
        .or(existing)
        .map(str::to_owned)
}

fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return false;
    };
    signature.len() % 4 == 0
        && regex::Regex::new(r"^[A-Za-z0-9+/]+={0,2}$")
            .expect("static regular expression")
            .is_match(signature)
}

fn resolve_thought_signature(
    same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    (same_provider_and_model && is_valid_thought_signature(signature))
        .then(|| signature.expect("validated signature").to_owned())
}

pub fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || get_gemini_major_version(model_id).is_some_and(|major| major >= 3.0)
}

fn get_gemini_major_version(model_id: &str) -> Option<f64> {
    let captures = regex::Regex::new(r"(?i)^gemini(?:-live)?-(\d+)")
        .expect("static regular expression")
        .captures(model_id)?;
    captures.get(1)?.as_str().parse().ok()
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    get_gemini_major_version(model_id).is_none_or(|major| major >= 3.0)
}

fn normalize_google_tool_call_id(id: &str, model: &Model) -> String {
    if !requires_tool_call_id(&model.id) {
        return id.to_owned();
    }
    let units = id
        .encode_utf16()
        .map(|unit| {
            if unit <= 0x7f
                && (char::from_u32(u32::from(unit)).is_some_and(|character| {
                    character.is_ascii_alphanumeric() || character == '_' || character == '-'
                }))
            {
                unit
            } else {
                u16::from(b'_')
            }
        })
        .take(64)
        .collect::<Vec<_>>();
    String::from_utf16(&units).expect("normalization emits ASCII")
}

pub fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let normalize = |id: &str, target: &Model, _source: &crate::types::AssistantMessage| {
        normalize_google_tool_call_id(id, target)
    };
    let messages = transform_messages(&context.messages, model, Some(&normalize));
    let mut contents = Vec::new();

    for message in messages {
        match message {
            Message::User(message) => {
                let parts = match &message.content {
                    UserContent::Text(text) => vec![json!({ "text": sanitize_surrogates(text) })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            UserContentBlock::Text(text) => {
                                Some(json!({ "text": sanitize_surrogates(&text.text) }))
                            }
                            UserContentBlock::Image(image) => Some(json!({
                                "inlineData": {
                                    "mimeType": image.mime_type,
                                    "data": image.data,
                                }
                            })),
                            UserContentBlock::Unknown(_) => None,
                        })
                        .collect(),
                };
                if matches!(&message.content, UserContent::Blocks(_)) && parts.is_empty() {
                    continue;
                }
                contents.push(json!({ "role": "user", "parts": parts }));
            }
            Message::Assistant(message) => {
                let same_provider_and_model =
                    message.provider == model.provider && message.model == model.id;
                let mut parts = Vec::new();
                for block in &message.content {
                    match block {
                        AssistantContent::Text(text) => {
                            let signature = resolve_thought_signature(
                                same_provider_and_model,
                                text.text_signature.as_deref(),
                            );
                            if text.text.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            let mut part = Map::from_iter([(
                                "text".to_owned(),
                                Value::String(sanitize_surrogates(&text.text)),
                            )]);
                            if let Some(signature) = signature {
                                part.insert(
                                    "thoughtSignature".to_owned(),
                                    Value::String(signature),
                                );
                            }
                            parts.push(Value::Object(part));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if same_provider_and_model {
                                let signature = resolve_thought_signature(
                                    true,
                                    thinking.thinking_signature.as_deref(),
                                );
                                if thinking.thinking.trim().is_empty() && signature.is_none() {
                                    continue;
                                }
                                let mut part = Map::from_iter([
                                    ("thought".to_owned(), Value::Bool(true)),
                                    (
                                        "text".to_owned(),
                                        Value::String(sanitize_surrogates(&thinking.thinking)),
                                    ),
                                ]);
                                if let Some(signature) = signature {
                                    part.insert(
                                        "thoughtSignature".to_owned(),
                                        Value::String(signature),
                                    );
                                }
                                parts.push(Value::Object(part));
                            } else if !thinking.thinking.trim().is_empty() {
                                parts.push(
                                    json!({ "text": sanitize_surrogates(&thinking.thinking) }),
                                );
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut function_call = Map::from_iter([
                                ("name".to_owned(), Value::String(tool_call.name.clone())),
                                (
                                    "args".to_owned(),
                                    if tool_call.arguments.is_null() {
                                        Value::Object(Map::new())
                                    } else {
                                        tool_call.arguments.clone()
                                    },
                                ),
                            ]);
                            if requires_tool_call_id(&model.id) {
                                function_call
                                    .insert("id".to_owned(), Value::String(tool_call.id.clone()));
                            }
                            let mut part = Map::from_iter([(
                                "functionCall".to_owned(),
                                Value::Object(function_call),
                            )]);
                            if let Some(signature) = resolve_thought_signature(
                                same_provider_and_model,
                                tool_call.thought_signature.as_deref(),
                            ) {
                                part.insert(
                                    "thoughtSignature".to_owned(),
                                    Value::String(signature),
                                );
                            }
                            parts.push(Value::Object(part));
                        }
                        AssistantContent::Unknown(_) => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Message::ToolResult(message) => {
                let text_result = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        UserContentBlock::Text(text) => Some(text.text.as_str()),
                        UserContentBlock::Image(_) | UserContentBlock::Unknown(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images = if model.input.contains(&ModelInput::Image) {
                    message
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            UserContentBlock::Image(image) => Some(json!({
                                "inlineData": {
                                    "mimeType": image.mime_type,
                                    "data": image.data,
                                }
                            })),
                            UserContentBlock::Text(_) => None,
                            UserContentBlock::Unknown(_) => None,
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let response_value = if !text_result.is_empty() {
                    sanitize_surrogates(&text_result)
                } else if !images.is_empty() {
                    "(see attached image)".to_owned()
                } else {
                    String::new()
                };
                let multimodal = supports_multimodal_function_response(&model.id);
                let mut function_response = Map::from_iter([
                    ("name".to_owned(), Value::String(message.tool_name.clone())),
                    (
                        "response".to_owned(),
                        if message.is_error {
                            json!({ "error": response_value })
                        } else {
                            json!({ "output": response_value })
                        },
                    ),
                ]);
                if !images.is_empty() && multimodal {
                    function_response.insert("parts".to_owned(), Value::Array(images.clone()));
                }
                if requires_tool_call_id(&model.id) {
                    function_response
                        .insert("id".to_owned(), Value::String(message.tool_call_id.clone()));
                }
                let response_part = json!({ "functionResponse": function_response });
                let merge = contents
                    .last_mut()
                    .and_then(Value::as_object_mut)
                    .is_some_and(|last| {
                        last.get("role").and_then(Value::as_str) == Some("user")
                            && last
                                .get("parts")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts
                                        .iter()
                                        .any(|part| part.get("functionResponse").is_some())
                                })
                    });
                if merge {
                    contents
                        .last_mut()
                        .and_then(|content| content.get_mut("parts"))
                        .and_then(Value::as_array_mut)
                        .expect("checked merge target")
                        .push(response_part);
                } else {
                    contents.push(json!({ "role": "user", "parts": [response_part] }));
                }
                if !images.is_empty() && !multimodal {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(images);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }
    contents
}

fn sanitize_for_open_api(schema: &Value) -> Value {
    let Value::Object(object) = schema else {
        return schema.clone();
    };
    Value::Object(
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
            .map(|(key, value)| (key.clone(), sanitize_for_open_api(value)))
            .collect(),
    )
}

pub fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict_mode: bool,
) -> Result<Option<Vec<Value>>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let declarations = tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)
                .map_err(|error| error.to_string())?;
            let parameters =
                get_json_schema_tool_parameters(tool, strict).map_err(|error| error.to_string())?;
            let mut declaration = Map::from_iter([
                ("name".to_owned(), Value::String(tool.name.clone())),
                (
                    "description".to_owned(),
                    Value::String(tool.description.clone()),
                ),
            ]);
            declaration.insert(
                if use_parameters {
                    "parameters"
                } else {
                    "parametersJsonSchema"
                }
                .to_owned(),
                if use_parameters {
                    sanitize_for_open_api(&parameters)
                } else {
                    parameters
                },
            );
            Ok(Value::Object(declaration))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Some(vec![json!({ "functionDeclarations": declarations })]))
}

pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    get_gemini_major_version(model_id).is_some_and(|major| major >= 3.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GoogleFunctionCallingMode {
    Auto,
    None,
    Any,
    Validated,
}

pub fn map_tool_choice(choice: &str) -> GoogleFunctionCallingMode {
    match choice {
        "none" => GoogleFunctionCallingMode::None,
        "any" => GoogleFunctionCallingMode::Any,
        _ => GoogleFunctionCallingMode::Auto,
    }
}

pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<GoogleFunctionCallingMode>, String> {
    let mut use_strict_mode = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict_mode)
            .map_err(|error| error.to_string())?
            == Some(true)
        {
            use_strict_mode = true;
            break;
        }
    }
    if matches!(tool_choice, Some("none" | "any")) {
        return Ok(tool_choice.map(map_tool_choice));
    }
    if use_strict_mode {
        return Ok(Some(GoogleFunctionCallingMode::Validated));
    }
    Ok(tool_choice.map(map_tool_choice))
}

pub fn map_stop_reason_string(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

pub fn map_stop_reason(reason: &adk_gemini::FinishReason) -> StopReason {
    match reason {
        adk_gemini::FinishReason::Stop => StopReason::Stop,
        adk_gemini::FinishReason::MaxTokens => StopReason::Length,
        adk_gemini::FinishReason::FinishReasonUnspecified
        | adk_gemini::FinishReason::Safety
        | adk_gemini::FinishReason::Recitation
        | adk_gemini::FinishReason::Language
        | adk_gemini::FinishReason::Other
        | adk_gemini::FinishReason::Blocklist
        | adk_gemini::FinishReason::ProhibitedContent
        | adk_gemini::FinishReason::Spii
        | adk_gemini::FinishReason::MalformedFunctionCall
        | adk_gemini::FinishReason::ModelArmor
        | adk_gemini::FinishReason::ImageSafety
        | adk_gemini::FinishReason::UnexpectedToolCall
        | adk_gemini::FinishReason::TooManyToolCalls => StopReason::Error,
    }
}

#[derive(Debug)]
pub struct GoogleSdkError {
    source: Option<adk_gemini::ClientError>,
    metadata: Option<ProviderErrorMetadata>,
}

impl GoogleSdkError {
    pub fn new(source: adk_gemini::ClientError) -> Self {
        let metadata = match &source {
            adk_gemini::ClientError::BadResponse { code, .. } => Some(ProviderErrorMetadata {
                status: Some(*code),
                headers: Default::default(),
            }),
            _ => None,
        };
        Self {
            source: Some(source),
            metadata,
        }
    }

    pub(crate) fn aborted() -> Self {
        Self {
            source: None,
            metadata: None,
        }
    }
}

impl std::fmt::Display for GoogleSdkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            Some(source) => source.fmt(formatter),
            None => formatter.write_str("Request aborted"),
        }
    }
}

impl std::error::Error for GoogleSdkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl ProviderRetryClassify for GoogleSdkError {
    fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata> {
        self.metadata.as_ref()
    }

    fn provider_error_message(&self) -> String {
        self.to_string()
    }
}

pub async fn retry_google_request<T, F, Fut>(
    request: F,
    options: ProviderRetryOptions,
) -> Result<T, ProviderRetryError<GoogleSdkError>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, GoogleSdkError>>,
{
    retry_provider_request(request, options).await
}

#[cfg(test)]
mod tests;
