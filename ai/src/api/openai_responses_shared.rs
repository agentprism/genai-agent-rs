//! Shared OpenAI Responses lowering and semantic event processing.
//!
//! This module owns no I/O. It mirrors pi `src/api/openai-responses-shared.ts` and is
//! shared by ordinary Responses and the later Codex transports.

use crate::api::constrained_sampling::{
    GrammarConstrainedSamplingFormat, GrammarToolInputJsonBuffer,
    append_grammar_tool_input_json_delta, get_grammar_tool_input, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::api::transform_messages::transform_messages;
use crate::event_stream::{AssistantMessageEvent, AssistantStreamSender};
use crate::models::calculate_cost;
use crate::types::{
    AssistantContent, AssistantMessage, ImageContent, Message, Model, ModelInput, TextContent,
    TextSignaturePhase, TextSignatureV1, ThinkingContent, Tool, ToolCall, ToolResultContent, Usage,
    UserContent, UserContentBlock,
};
use crate::utils::hash::short_hash;
use crate::utils::json_parse::parse_streaming_json;
use crate::utils::sanitize_unicode::sanitize_surrogates;
use futures::{Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct OpenAIResponsesError(String);

impl OpenAIResponsesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseReasoningSummary {
    Auto,
    Detailed,
    Concise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseToolChoiceMode {
    None,
    Auto,
    Required,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseToolChoice {
    Mode(ResponseToolChoiceMode),
    Allowed {
        #[serde(rename = "type")]
        kind: AllowedToolsType,
        mode: AllowedToolsMode,
        tools: Vec<Map<String, Value>>,
    },
    Named {
        #[serde(rename = "type")]
        kind: NamedToolChoiceType,
        name: String,
    },
    Mcp {
        #[serde(rename = "type")]
        kind: McpToolChoiceType,
        server_label: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_present_option"
        )]
        name: Option<Option<String>>,
    },
    Hosted {
        #[serde(rename = "type")]
        kind: HostedToolChoiceType,
    },
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowedToolsType {
    #[serde(rename = "allowed_tools")]
    AllowedTools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllowedToolsMode {
    Auto,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedToolChoiceType {
    Function,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpToolChoiceType {
    #[serde(rename = "mcp")]
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedToolChoiceType {
    FileSearch,
    WebSearchPreview,
    Computer,
    ComputerUsePreview,
    ComputerUse,
    #[serde(rename = "web_search_preview_2025_03_11")]
    WebSearchPreview2025_03_11,
    ImageGeneration,
    CodeInterpreter,
    Mcp,
    ApplyPatch,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseInstructionRole {
    System,
    Developer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseUserRole {
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseAssistantRole {
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputContent {
    InputText {
        text: String,
    },
    InputImage {
        detail: ResponseImageDetail,
        image_url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseImageDetail {
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseMessageContent {
    OutputText {
        text: String,
        annotations: Vec<ResponseMessageAnnotation>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ResponseMessageAnnotation {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseInstructionMessage {
    pub role: ResponseInstructionRole,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseUserMessage {
    pub role: ResponseUserRole,
    pub content: Vec<ResponseInputContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseOutputMessageInput {
    #[serde(rename = "type")]
    pub kind: ResponseMessageType,
    pub role: ResponseAssistantRole,
    pub content: Vec<ResponseMessageContent>,
    pub status: ResponseItemStatus,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TextSignaturePhase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseMessageType {
    #[serde(rename = "message")]
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseItemStatus {
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseFunctionCallInput {
    #[serde(rename = "type")]
    pub kind: ResponseFunctionCallType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseFunctionCallType {
    #[serde(rename = "function_call")]
    FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseCustomToolCallInput {
    #[serde(rename = "type")]
    pub kind: ResponseCustomToolCallType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub name: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseCustomToolCallType {
    #[serde(rename = "custom_tool_call")]
    CustomToolCall,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolResultOutput {
    Text(String),
    Content(Vec<ResponseInputContent>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseToolCallOutput {
    #[serde(rename = "type")]
    pub kind: ResponseToolCallOutputType,
    pub call_id: String,
    pub output: ToolResultOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseToolCallOutputType {
    FunctionCallOutput,
    CustomToolCallOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseAdditionalToolsInput {
    #[serde(rename = "type")]
    pub kind: ResponseAdditionalToolsType,
    pub role: ResponseInstructionRole,
    pub tools: Vec<ResponseTool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseAdditionalToolsType {
    #[serde(rename = "additional_tools")]
    AdditionalTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseToolSearchArguments {
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseToolSearchCallInput {
    #[serde(rename = "type")]
    pub kind: ResponseToolSearchCallType,
    pub call_id: String,
    pub execution: ResponseToolSearchExecution,
    pub status: ResponseItemStatus,
    pub arguments: ResponseToolSearchArguments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseToolSearchCallType {
    #[serde(rename = "tool_search_call")]
    ToolSearchCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseToolSearchExecution {
    Client,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseToolSearchOutputInput {
    #[serde(rename = "type")]
    pub kind: ResponseToolSearchOutputType,
    pub call_id: String,
    pub execution: ResponseToolSearchExecution,
    pub status: ResponseItemStatus,
    pub tools: Vec<ResponseTool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseToolSearchOutputType {
    #[serde(rename = "tool_search_output")]
    ToolSearchOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ResponseInputItem {
    Instruction(ResponseInstructionMessage),
    User(ResponseUserMessage),
    Reasoning(Value),
    OutputMessage(ResponseOutputMessageInput),
    FunctionCall(ResponseFunctionCallInput),
    CustomToolCall(ResponseCustomToolCallInput),
    ToolCallOutput(ResponseToolCallOutput),
    AdditionalTools(ResponseAdditionalToolsInput),
    ToolSearchCall(ResponseToolSearchCallInput),
    ToolSearchOutput(ResponseToolSearchOutputInput),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ResponseTool {
    Function {
        name: String,
        description: String,
        parameters: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<Option<bool>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
    },
    Custom {
        name: String,
        description: String,
        format: ResponseCustomToolFormat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defer_loading: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseCustomToolFormat {
    #[serde(rename = "type")]
    pub kind: ResponseGrammarType,
    pub syntax: ResponseGrammarSyntax,
    pub definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ResponseGrammarType {
    #[serde(rename = "grammar")]
    Grammar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Debug, Clone, Default)]
pub struct ConvertResponsesToolsOptions {
    /// Outer `None` is omitted; `Some(None)` is explicit JSON null (pi
    /// `openai-responses-shared.ts:126-130`).
    pub strict: Option<Option<bool>>,
    pub supports_strict_mode: Option<bool>,
    pub supports_open_ai_grammar_tools: Option<bool>,
    pub defer_loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredResponsesToolsMode {
    AdditionalTools,
    ToolSearch,
}

#[derive(Debug, Clone, Default)]
pub struct ConvertResponsesMessagesOptions<'a> {
    pub include_system_prompt: Option<bool>,
    pub grammar_tool_input_properties: Option<&'a BTreeMap<String, String>>,
    pub deferred_tools: Option<&'a [(String, Tool)]>,
    pub deferred_tools_mode: Option<DeferredResponsesToolsMode>,
    pub tool_options: ConvertResponsesToolsOptions,
}

pub type ResolveResponsesServiceTier<'a> = dyn Fn(
        Option<Option<ResponseServiceTier>>,
        Option<Option<ResponseServiceTier>>,
    ) -> Option<Option<ResponseServiceTier>>
    + Send
    + Sync
    + 'a;
pub type ApplyResponsesServiceTierPricing<'a> =
    dyn Fn(&mut Usage, Option<Option<ResponseServiceTier>>) + Send + Sync + 'a;

#[derive(Default)]
pub struct OpenAIResponsesStreamOptions<'a> {
    pub service_tier: Option<Option<ResponseServiceTier>>,
    pub grammar_tool_input_properties: Option<&'a BTreeMap<String, String>>,
    pub resolve_service_tier: Option<&'a ResolveResponsesServiceTier<'a>>,
    pub apply_service_tier_pricing: Option<&'a ApplyResponsesServiceTierPricing<'a>>,
}

fn encode_text_signature_v1(id: String, phase: Option<TextSignaturePhase>) -> String {
    serde_json::to_string(&TextSignatureV1 { v: 1, id, phase })
        .expect("text signatures are serializable")
}

fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<TextSignaturePhase>)> {
    let signature = signature.filter(|signature| !signature.is_empty())?;
    if signature.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(signature)
        && value.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = value.get("id").and_then(Value::as_str)
    {
        let phase = value
            .get("phase")
            .and_then(|phase| serde_json::from_value(phase.clone()).ok());
        return Some((id.to_owned(), phase));
    }
    Some((signature.to_owned(), None))
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn normalize_id_part(part: &str) -> String {
    let mut normalized = part
        .encode_utf16()
        .map(|unit| {
            let byte = u8::try_from(unit).ok();
            if byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                char::from(byte.expect("checked above"))
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    format!("fc_{}", short_hash(item_id))
        .chars()
        .take(64)
        .collect()
}

fn normalize_tool_call_id(
    id: &str,
    target: &Model,
    source: &AssistantMessage,
    allowed_tool_call_providers: &BTreeSet<String>,
) -> String {
    if !allowed_tool_call_providers.contains(target.provider.as_str()) {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call_id = parts.next().unwrap_or_default();
    let Some(item_id) = parts.next() else {
        return normalize_id_part(id);
    };
    let normalized_call_id = normalize_id_part(call_id);
    let foreign = source.provider != target.provider || source.api != target.api;
    let mut normalized_item_id = if foreign {
        build_foreign_responses_item_id(item_id)
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

fn convert_tool_result_output(model: &Model, content: &[ToolResultContent]) -> ToolResultOutput {
    let text = content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text(text) => Some(text.text.as_str()),
            UserContentBlock::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Image(image) => Some(image),
            UserContentBlock::Text(_) => None,
        })
        .collect::<Vec<_>>();
    let has_text = !text.is_empty();
    if images.is_empty() || !model.input.contains(&ModelInput::Image) {
        let output = if has_text {
            text
        } else if images.is_empty() {
            "(no tool output)".to_owned()
        } else {
            "(see attached image)".to_owned()
        };
        return ToolResultOutput::Text(sanitize_surrogates(&output));
    }

    let mut output = Vec::with_capacity(images.len() + usize::from(has_text));
    if has_text {
        output.push(ResponseInputContent::InputText {
            text: sanitize_surrogates(&text),
        });
    }
    for image in images {
        output.push(response_input_image(image));
    }
    ToolResultOutput::Content(output)
}

fn response_input_image(image: &ImageContent) -> ResponseInputContent {
    ResponseInputContent::InputImage {
        detail: ResponseImageDetail::Auto,
        image_url: format!("data:{};base64,{}", image.mime_type, image.data),
    }
}

pub fn convert_responses_messages(
    model: &Model,
    context: &crate::types::Context,
    allowed_tool_call_providers: &BTreeSet<String>,
    options: ConvertResponsesMessagesOptions<'_>,
) -> Result<Vec<ResponseInputItem>, OpenAIResponsesError> {
    let normalizer = |id: &str, target: &Model, source: &AssistantMessage| {
        normalize_tool_call_id(id, target, source, allowed_tool_call_providers)
    };
    let transformed = transform_messages(&context.messages, model, Some(&normalizer));
    let mut messages = Vec::new();
    let mut loaded_tool_names = BTreeSet::new();

    if options.include_system_prompt.unwrap_or(true)
        && let Some(system_prompt) = context
            .system_prompt
            .as_ref()
            .filter(|prompt| !prompt.is_empty())
    {
        let supports_developer = match model.compat.as_ref() {
            Some(crate::types::ModelCompat::OpenAIResponses(compat)) => {
                compat.supports_developer_role.unwrap_or(true)
            }
            _ => true,
        };
        messages.push(ResponseInputItem::Instruction(ResponseInstructionMessage {
            role: if model.reasoning && supports_developer {
                ResponseInstructionRole::Developer
            } else {
                ResponseInstructionRole::System
            },
            content: sanitize_surrogates(system_prompt),
        }));
    }

    let mut message_index = 0_usize;
    for message in &transformed {
        match message {
            Message::User(message) => {
                let user_start = messages.len();
                let content = match &message.content {
                    UserContent::Text(text) => vec![ResponseInputContent::InputText {
                        text: sanitize_surrogates(text),
                    }],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|block| match block {
                            UserContentBlock::Text(text) => ResponseInputContent::InputText {
                                text: sanitize_surrogates(&text.text),
                            },
                            UserContentBlock::Image(image) => response_input_image(image),
                        })
                        .collect(),
                };
                if !content.is_empty() {
                    messages.push(ResponseInputItem::User(ResponseUserMessage {
                        role: ResponseUserRole::User,
                        content,
                    }));
                }
                if messages.len() == user_start {
                    continue;
                }
            }
            Message::Assistant(assistant) => {
                let output_start = messages.len();
                let same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let same_model = same_provider_and_api && assistant.model == model.id;
                let different_model = same_provider_and_api && assistant.model != model.id;
                let mut text_block_index = 0_usize;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = thinking
                                .thinking_signature
                                .as_deref()
                                .filter(|signature| !signature.is_empty())
                            {
                                let reasoning = serde_json::from_str(signature)
                                    .map_err(OpenAIResponsesError::display)?;
                                messages.push(ResponseInputItem::Reasoning(reasoning));
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed = parse_text_signature(text.text_signature.as_deref());
                            let fallback = if text_block_index == 0 {
                                format!("msg_pi_{message_index}")
                            } else {
                                format!("msg_pi_{message_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let (mut id, phase) = parsed.unwrap_or((fallback, None));
                            if id.is_empty() {
                                id = if text_block_index == 1 {
                                    format!("msg_pi_{message_index}")
                                } else {
                                    format!("msg_pi_{message_index}_{}", text_block_index - 1)
                                };
                            } else if utf16_len(&id) > 64 {
                                id = format!("msg_{}", short_hash(&id));
                            }
                            messages.push(ResponseInputItem::OutputMessage(
                                ResponseOutputMessageInput {
                                    kind: ResponseMessageType::Message,
                                    role: ResponseAssistantRole::Assistant,
                                    content: vec![ResponseMessageContent::OutputText {
                                        text: sanitize_surrogates(&text.text),
                                        annotations: Vec::new(),
                                    }],
                                    status: ResponseItemStatus::Completed,
                                    id,
                                    phase,
                                },
                            ));
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut id_parts = tool_call.id.split('|');
                            let call_id = id_parts.next().unwrap_or_default().to_owned();
                            let mut item_id = id_parts.next().map(str::to_owned);
                            let custom_input_property = options
                                .grammar_tool_input_properties
                                .and_then(|properties| properties.get(&tool_call.name));
                            if (different_model
                                && item_id.as_deref().is_some_and(|id| id.starts_with("fc_")))
                                || (custom_input_property.is_none()
                                    && !item_id.as_deref().is_some_and(|id| id.starts_with("fc_")))
                            {
                                item_id = None;
                            }
                            let deferred_contains = options.deferred_tools.is_some_and(|tools| {
                                tools.iter().any(|(name, _)| name == &tool_call.name)
                            });
                            let namespace = (same_model || deferred_contains)
                                .then(|| tool_call.namespace.clone())
                                .flatten();
                            if let Some(input_property) = custom_input_property {
                                messages.push(ResponseInputItem::CustomToolCall(
                                    ResponseCustomToolCallInput {
                                        kind: ResponseCustomToolCallType::CustomToolCall,
                                        id: item_id,
                                        call_id,
                                        name: tool_call.name.clone(),
                                        input: sanitize_surrogates(
                                            &get_grammar_tool_input(
                                                &tool_call.name,
                                                &tool_call.arguments,
                                                input_property,
                                            )
                                            .map_err(OpenAIResponsesError::display)?,
                                        ),
                                        namespace,
                                    },
                                ));
                            } else {
                                messages.push(ResponseInputItem::FunctionCall(
                                    ResponseFunctionCallInput {
                                        kind: ResponseFunctionCallType::FunctionCall,
                                        id: item_id,
                                        call_id,
                                        name: tool_call.name.clone(),
                                        arguments: serde_json::to_string(&tool_call.arguments)
                                            .map_err(OpenAIResponsesError::display)?,
                                        namespace,
                                    },
                                ));
                            }
                        }
                    }
                }
                if messages.len() == output_start {
                    continue;
                }
            }
            Message::ToolResult(tool_result) => {
                let call_id = tool_result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let kind = if options
                    .grammar_tool_input_properties
                    .is_some_and(|properties| properties.contains_key(&tool_result.tool_name))
                {
                    ResponseToolCallOutputType::CustomToolCallOutput
                } else {
                    ResponseToolCallOutputType::FunctionCallOutput
                };
                messages.push(ResponseInputItem::ToolCallOutput(ResponseToolCallOutput {
                    kind,
                    call_id,
                    output: convert_tool_result_output(model, &tool_result.content),
                }));

                let deferred = tool_result
                    .added_tool_names
                    .iter()
                    .flatten()
                    .filter_map(|name| {
                        let tool = options
                            .deferred_tools?
                            .iter()
                            .find(|(tool_name, _)| tool_name == name)?;
                        loaded_tool_names.insert(name.clone()).then(|| tool.clone())
                    })
                    .collect::<Vec<_>>();
                match (deferred.is_empty(), options.deferred_tools_mode) {
                    (true, _) | (false, None) => {}
                    (false, Some(DeferredResponsesToolsMode::AdditionalTools)) => {
                        messages.push(ResponseInputItem::AdditionalTools(
                            ResponseAdditionalToolsInput {
                                kind: ResponseAdditionalToolsType::AdditionalTools,
                                role: ResponseInstructionRole::Developer,
                                tools: convert_responses_tools(
                                    deferred.iter().map(|(_, tool)| tool),
                                    &options.tool_options,
                                )?,
                            },
                        ));
                    }
                    (false, Some(DeferredResponsesToolsMode::ToolSearch)) => {
                        let names = deferred
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>();
                        let search_call_id = format!(
                            "pi_tool_load_{}",
                            short_hash(&format!(
                                "{}:{}",
                                tool_result.tool_call_id,
                                names.join(",")
                            ))
                        );
                        messages.push(ResponseInputItem::ToolSearchCall(
                            ResponseToolSearchCallInput {
                                kind: ResponseToolSearchCallType::ToolSearchCall,
                                call_id: search_call_id.clone(),
                                execution: ResponseToolSearchExecution::Client,
                                status: ResponseItemStatus::Completed,
                                arguments: ResponseToolSearchArguments {
                                    query: names.join(" "),
                                    limit: names.len(),
                                },
                            },
                        ));
                        let mut tool_options = options.tool_options.clone();
                        tool_options.defer_loading = true;
                        messages.push(ResponseInputItem::ToolSearchOutput(
                            ResponseToolSearchOutputInput {
                                kind: ResponseToolSearchOutputType::ToolSearchOutput,
                                call_id: search_call_id,
                                execution: ResponseToolSearchExecution::Client,
                                status: ResponseItemStatus::Completed,
                                tools: convert_responses_tools(
                                    deferred.iter().map(|(_, tool)| tool),
                                    &tool_options,
                                )?,
                            },
                        ));
                    }
                }
            }
        }
        message_index += 1;
    }
    Ok(messages)
}

pub fn convert_responses_tools<'a>(
    tools: impl IntoIterator<Item = &'a Tool>,
    options: &ConvertResponsesToolsOptions,
) -> Result<Vec<ResponseTool>, OpenAIResponsesError> {
    let default_strict = options.strict.unwrap_or(Some(false));
    let supports_strict_mode = options.supports_strict_mode.unwrap_or(true);
    let supports_grammar = options.supports_open_ai_grammar_tools.unwrap_or(false);
    tools
        .into_iter()
        .map(|tool| {
            if let Some(grammar) = resolve_grammar_constrained_sampling(tool, supports_grammar)
                .map_err(OpenAIResponsesError::display)?
            {
                return Ok(ResponseTool::Custom {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    format: ResponseCustomToolFormat {
                        kind: ResponseGrammarType::Grammar,
                        syntax: match grammar.format {
                            GrammarConstrainedSamplingFormat::Lark => ResponseGrammarSyntax::Lark,
                            GrammarConstrainedSamplingFormat::Regex => ResponseGrammarSyntax::Regex,
                        },
                        definition: grammar.definition,
                    },
                    defer_loading: options.defer_loading.then_some(true),
                });
            }
            let constrained = resolve_json_schema_strict_sampling(tool, supports_strict_mode)
                .map_err(OpenAIResponsesError::display)?;
            let strict = constrained.map_or(default_strict, Some);
            Ok(ResponseTool::Function {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: get_json_schema_tool_parameters(tool, strict)
                    .map_err(OpenAIResponsesError::display)?,
                strict: supports_strict_mode.then_some(strict),
                defer_loading: options.defer_loading.then_some(true),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct CreatedEvent {
    response: CreatedResponse,
}

#[derive(Debug, Deserialize)]
struct CreatedResponse {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputItemEvent {
    #[serde(default)]
    output_index: u64,
    item: Value,
}

#[derive(Debug, Deserialize)]
struct DeltaEvent {
    #[serde(default)]
    output_index: u64,
    delta: String,
}

#[derive(Debug, Deserialize)]
struct ReasoningPartDoneEvent {
    #[serde(default)]
    output_index: u64,
}

#[derive(Debug, Deserialize)]
struct FunctionArgumentsDoneEvent {
    #[serde(default)]
    output_index: u64,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct CustomInputDoneEvent {
    #[serde(default)]
    output_index: u64,
    input: String,
}

#[derive(Debug, Deserialize)]
struct TerminalEvent {
    response: TerminalResponse,
}

#[derive(Debug, Deserialize)]
struct TerminalResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ResponseFailure>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    service_tier: Option<Option<ResponseServiceTier>>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<Value>,
}

impl IncompleteDetails {
    fn string_reason(&self) -> Option<&str> {
        self.reason.as_ref().and_then(Value::as_str)
    }
}

#[derive(Debug, Deserialize)]
struct ResponseFailure {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<ResponseInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponseOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponseInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ResponseOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    #[serde(default, deserialize_with = "deserialize_present_option")]
    code: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    message: Option<Option<String>>,
}

#[derive(Debug)]
enum DecodedResponseEvent {
    Created(CreatedEvent),
    OutputItemAdded(OutputItemEvent),
    ReasoningSummaryDelta(DeltaEvent),
    ReasoningSummaryPartDone(ReasoningPartDoneEvent),
    ReasoningTextDelta(DeltaEvent),
    OutputTextDelta(DeltaEvent),
    RefusalDelta(DeltaEvent),
    FunctionArgumentsDelta(DeltaEvent),
    FunctionArgumentsDone(FunctionArgumentsDoneEvent),
    CustomInputDelta(DeltaEvent),
    CustomInputDone(CustomInputDoneEvent),
    OutputItemDone(OutputItemEvent),
    Completed(TerminalEvent),
    Incomplete(TerminalEvent),
    Failed(TerminalEvent),
    Error(ErrorEvent),
    Unknown,
}

fn decode_response_event(value: Value) -> Result<DecodedResponseEvent, OpenAIResponsesError> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Ok(DecodedResponseEvent::Unknown);
    };
    match kind {
        "response.created" => decode_event(value).map(DecodedResponseEvent::Created),
        "response.output_item.added" => {
            decode_event(value).map(DecodedResponseEvent::OutputItemAdded)
        }
        "response.reasoning_summary_text.delta" => {
            decode_event(value).map(DecodedResponseEvent::ReasoningSummaryDelta)
        }
        "response.reasoning_summary_part.done" => {
            decode_event(value).map(DecodedResponseEvent::ReasoningSummaryPartDone)
        }
        "response.reasoning_text.delta" => {
            decode_event(value).map(DecodedResponseEvent::ReasoningTextDelta)
        }
        "response.output_text.delta" => {
            decode_event(value).map(DecodedResponseEvent::OutputTextDelta)
        }
        "response.refusal.delta" => decode_event(value).map(DecodedResponseEvent::RefusalDelta),
        "response.function_call_arguments.delta" => {
            decode_event(value).map(DecodedResponseEvent::FunctionArgumentsDelta)
        }
        "response.function_call_arguments.done" => {
            decode_event(value).map(DecodedResponseEvent::FunctionArgumentsDone)
        }
        "response.custom_tool_call_input.delta" => {
            decode_event(value).map(DecodedResponseEvent::CustomInputDelta)
        }
        "response.custom_tool_call_input.done" => {
            decode_event(value).map(DecodedResponseEvent::CustomInputDone)
        }
        "response.output_item.done" => {
            decode_event(value).map(DecodedResponseEvent::OutputItemDone)
        }
        "response.completed" => decode_event(value).map(DecodedResponseEvent::Completed),
        "response.incomplete" => decode_event(value).map(DecodedResponseEvent::Incomplete),
        "response.failed" => decode_event(value).map(DecodedResponseEvent::Failed),
        "error" => decode_event(value).map(DecodedResponseEvent::Error),
        _ => Ok(DecodedResponseEvent::Unknown),
    }
}

fn decode_event<T: DeserializeOwned>(value: Value) -> Result<T, OpenAIResponsesError> {
    serde_json::from_value(value).map_err(OpenAIResponsesError::display)
}

#[derive(Debug, Deserialize)]
struct ReasoningItem {
    id: String,
    #[serde(default)]
    summary: Vec<ReasoningText>,
    #[serde(default)]
    content: Vec<ReasoningText>,
    #[serde(default)]
    encrypted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReasoningText {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct MessageItem {
    id: String,
    #[serde(default)]
    phase: Option<TextSignaturePhase>,
    #[serde(default)]
    content: Vec<OutputMessageContent>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputMessageContent {
    OutputText {
        #[serde(default)]
        text: String,
    },
    Refusal {
        #[serde(default)]
        refusal: String,
    },
}

#[derive(Debug, Deserialize)]
struct FunctionCallItem {
    #[serde(default)]
    id: Option<String>,
    call_id: String,
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CustomToolCallItem {
    #[serde(default)]
    id: Option<String>,
    call_id: String,
    name: String,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug)]
enum OutputItem {
    Reasoning { raw: Value, item: ReasoningItem },
    Message(MessageItem),
    FunctionCall(FunctionCallItem),
    CustomToolCall(CustomToolCallItem),
    Unknown,
}

fn decode_output_item(value: Value) -> Result<OutputItem, OpenAIResponsesError> {
    match value.get("type").and_then(Value::as_str) {
        Some("reasoning") => serde_json::from_value(value.clone())
            .map(|item| OutputItem::Reasoning { raw: value, item })
            .map_err(OpenAIResponsesError::display),
        Some("message") => serde_json::from_value(value)
            .map(OutputItem::Message)
            .map_err(OpenAIResponsesError::display),
        Some("function_call") => serde_json::from_value(value)
            .map(OutputItem::FunctionCall)
            .map_err(OpenAIResponsesError::display),
        Some("custom_tool_call") => serde_json::from_value(value)
            .map(OutputItem::CustomToolCall)
            .map_err(OpenAIResponsesError::display),
        _ => Ok(OutputItem::Unknown),
    }
}

#[derive(Debug)]
enum OutputSlot {
    Thinking {
        content_index: usize,
    },
    Text {
        content_index: usize,
    },
    FunctionCall {
        content_index: usize,
        partial_json: String,
    },
    CustomToolCall {
        content_index: usize,
        property: String,
        json_buffer: GrammarToolInputJsonBuffer,
    },
}

fn apply_message_phase_stop_reason(item: &OutputItem, output: &mut AssistantMessage) {
    if matches!(
        item,
        OutputItem::Message(MessageItem {
            phase: Some(TextSignaturePhase::FinalAnswer),
            ..
        })
    ) {
        output.stop_reason = crate::types::StopReason::Stop;
    }
}

fn tool_call_mut(output: &mut AssistantMessage, index: usize) -> Option<&mut ToolCall> {
    match output.content.get_mut(index) {
        Some(AssistantContent::ToolCall(tool_call)) => Some(tool_call),
        _ => None,
    }
}

fn text_mut(output: &mut AssistantMessage, index: usize) -> Option<&mut TextContent> {
    match output.content.get_mut(index) {
        Some(AssistantContent::Text(text)) => Some(text),
        _ => None,
    }
}

fn thinking_mut(output: &mut AssistantMessage, index: usize) -> Option<&mut ThinkingContent> {
    match output.content.get_mut(index) {
        Some(AssistantContent::Thinking(thinking)) => Some(thinking),
        _ => None,
    }
}

fn tool_arguments(raw: &str) -> Map<String, Value> {
    parse_streaming_json(Some(raw))
        .as_object()
        .cloned()
        .unwrap_or_default()
}

fn composite_tool_call_id(call_id: &str, item_id: Option<&str>) -> String {
    format!("{call_id}|{}", item_id.unwrap_or("undefined"))
}

fn create_slot(
    output_index: u64,
    item: &OutputItem,
    output: &mut AssistantMessage,
    sender: &AssistantStreamSender,
    slots: &mut BTreeMap<u64, OutputSlot>,
    grammar_properties: Option<&BTreeMap<String, String>>,
) -> Result<(), OpenAIResponsesError> {
    let slot = match item {
        OutputItem::Reasoning { .. } => {
            let content_index = output.content.len();
            output
                .content
                .push(AssistantContent::Thinking(ThinkingContent::new("")));
            sender
                .send(AssistantMessageEvent::ThinkingStart { content_index })
                .map_err(OpenAIResponsesError::display)?;
            OutputSlot::Thinking { content_index }
        }
        OutputItem::Message(_) => {
            apply_message_phase_stop_reason(item, output);
            let content_index = output.content.len();
            output
                .content
                .push(AssistantContent::Text(TextContent::new("")));
            sender
                .send(AssistantMessageEvent::TextStart { content_index })
                .map_err(OpenAIResponsesError::display)?;
            OutputSlot::Text { content_index }
        }
        OutputItem::FunctionCall(item) => {
            let content_index = output.content.len();
            let id = composite_tool_call_id(&item.call_id, item.id.as_deref());
            let mut tool_call = ToolCall::new(&id, &item.name, Map::new());
            tool_call.namespace.clone_from(&item.namespace);
            output.content.push(AssistantContent::ToolCall(tool_call));
            sender
                .send(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    id,
                    tool_name: item.name.clone(),
                })
                .map_err(OpenAIResponsesError::display)?;
            OutputSlot::FunctionCall {
                content_index,
                partial_json: item.arguments.clone(),
            }
        }
        OutputItem::CustomToolCall(item) => {
            let content_index = output.content.len();
            let id = composite_tool_call_id(&item.call_id, item.id.as_deref());
            let property = grammar_properties
                .and_then(|properties| properties.get(&item.name))
                .cloned()
                .unwrap_or_else(|| "input".to_owned());
            let mut arguments = Map::new();
            arguments.insert(
                property.clone(),
                Value::String(item.input.clone().unwrap_or_default()),
            );
            let mut tool_call = ToolCall::new(&id, &item.name, arguments);
            tool_call.namespace.clone_from(&item.namespace);
            output.content.push(AssistantContent::ToolCall(tool_call));
            sender
                .send(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    id,
                    tool_name: item.name.clone(),
                })
                .map_err(OpenAIResponsesError::display)?;
            OutputSlot::CustomToolCall {
                content_index,
                property,
                json_buffer: GrammarToolInputJsonBuffer::default(),
            }
        }
        OutputItem::Unknown => return Ok(()),
    };
    slots.insert(output_index, slot);
    Ok(())
}

fn push_tool_call_delta(
    sender: &AssistantStreamSender,
    content_index: usize,
    delta: Option<String>,
) -> Result<(), OpenAIResponsesError> {
    if let Some(delta) = delta {
        sender
            .send(AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
            })
            .map_err(OpenAIResponsesError::display)?;
    }
    Ok(())
}

fn custom_tool_input(output: &AssistantMessage, content_index: usize, property: &str) -> String {
    match output.content.get(content_index) {
        Some(AssistantContent::ToolCall(tool_call)) => tool_call
            .arguments
            .get(property)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn append_custom_tool_input(
    output: &mut AssistantMessage,
    content_index: usize,
    property: &str,
    buffer: &mut GrammarToolInputJsonBuffer,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, OpenAIResponsesError> {
    let delta = append_grammar_tool_input_json_delta(buffer, property, next_input, close)
        .map_err(OpenAIResponsesError::display)?;
    if let Some(tool_call) = tool_call_mut(output, content_index) {
        tool_call.arguments =
            Map::from_iter([(property.to_owned(), Value::String(next_input.to_owned()))]);
    }
    Ok(delta)
}

fn message_item_text(item: &MessageItem) -> String {
    item.content
        .iter()
        .map(|content| match content {
            OutputMessageContent::OutputText { text } => text.as_str(),
            OutputMessageContent::Refusal { refusal } => refusal.as_str(),
        })
        .collect()
}

fn js_nullable_string(value: Option<Option<String>>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(None) => "null".to_owned(),
        Some(Some(value)) => value,
    }
}

fn finalize_response(
    response: TerminalResponse,
    output: &mut AssistantMessage,
    model: &Model,
    reasoning_blocks_by_id: &BTreeMap<String, usize>,
    options: &OpenAIResponsesStreamOptions<'_>,
) -> Result<(), OpenAIResponsesError> {
    for raw in response.output {
        let OutputItem::Reasoning { item, .. } = decode_output_item(raw)? else {
            continue;
        };
        let Some(encrypted_content) = item.encrypted_content.filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Some(index) = reasoning_blocks_by_id.get(&item.id) else {
            continue;
        };
        let Some(block) = thinking_mut(output, *index) else {
            continue;
        };
        let Some(signature) = block.thinking_signature.as_deref() else {
            continue;
        };
        let Ok(mut stored) = serde_json::from_str::<Value>(signature) else {
            continue;
        };
        let Some(stored_object) = stored.as_object_mut() else {
            continue;
        };
        if stored_object
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            continue;
        }
        stored_object.insert(
            "encrypted_content".to_owned(),
            Value::String(encrypted_content),
        );
        block.thinking_signature = Some(
            serde_json::to_string(&stored).expect("reasoning signature value is serializable"),
        );
    }

    if let Some(id) = response.id {
        output.response_id = Some(id);
    }
    if let Some(usage) = response.usage {
        let cached = usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .unwrap_or(0);
        let cache_write = usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens)
            .unwrap_or(0);
        output.usage = Usage {
            input: usage
                .input_tokens
                .unwrap_or(0)
                .saturating_sub(cached)
                .saturating_sub(cache_write),
            output: usage.output_tokens.unwrap_or(0),
            cache_read: cached,
            cache_write,
            cache_write_1h: None,
            reasoning: Some(
                usage
                    .output_tokens_details
                    .as_ref()
                    .and_then(|details| details.reasoning_tokens)
                    .unwrap_or(0),
            ),
            total_tokens: usage.total_tokens.unwrap_or(0),
            cost: Default::default(),
        };
    }
    calculate_cost(model, &mut output.usage);
    if let Some(apply) = options.apply_service_tier_pricing {
        let response_tier = response.service_tier;
        let request_tier = options.service_tier;
        let service_tier = options.resolve_service_tier.map_or_else(
            || match response_tier {
                Some(Some(tier)) => Some(Some(tier)),
                Some(None) | None => request_tier,
            },
            |resolve| resolve(response_tier, request_tier),
        );
        apply(&mut output.usage, service_tier);
    }

    let incomplete_reason = response
        .incomplete_details
        .as_ref()
        .and_then(IncompleteDetails::string_reason);
    output.raw_stop_reason = match (response.status.as_deref(), incomplete_reason) {
        (Some(status), Some(reason)) => Some(format!("{status}.{reason}")),
        (Some(status), None) => Some(status.to_owned()),
        (None, _) => None,
    };
    let (stop_reason, error_message) =
        map_stop_reason(response.status.as_deref(), incomplete_reason)?;
    output.stop_reason = stop_reason;
    output.error_message = error_message;
    if output.stop_reason == crate::types::StopReason::Stop
        && output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        output.stop_reason = crate::types::StopReason::ToolUse;
    }
    Ok(())
}

fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> Result<(crate::types::StopReason, Option<String>), OpenAIResponsesError> {
    match status {
        None | Some("completed" | "in_progress" | "queued") => {
            Ok((crate::types::StopReason::Stop, None))
        }
        Some("incomplete") if incomplete_reason == Some("max_output_tokens") => {
            Ok((crate::types::StopReason::Length, None))
        }
        Some("incomplete") => Ok((
            crate::types::StopReason::Error,
            Some(incomplete_reason.map_or_else(
                || "Response incomplete without a provider reason".to_owned(),
                |reason| format!("Response incomplete: {reason}"),
            )),
        )),
        Some("failed" | "cancelled") => Ok((crate::types::StopReason::Error, None)),
        Some(status) => Err(OpenAIResponsesError::new(format!(
            "Unhandled stop reason: {status}"
        ))),
    }
}

pub async fn process_responses_stream<S, E>(
    openai_stream: &mut S,
    output: &mut AssistantMessage,
    sender: &AssistantStreamSender,
    model: &Model,
    options: OpenAIResponsesStreamOptions<'_>,
) -> Result<(), OpenAIResponsesError>
where
    S: Stream<Item = Result<Value, E>> + Unpin,
    E: fmt::Display,
{
    let mut saw_terminal_response_event = false;
    let mut output_slots = BTreeMap::<u64, OutputSlot>::new();
    let mut reasoning_blocks_by_id = BTreeMap::<String, usize>::new();

    while let Some(event) = openai_stream.next().await {
        let event = decode_response_event(event.map_err(OpenAIResponsesError::display)?)?;
        match event {
            DecodedResponseEvent::Created(event) => {
                if let Some(id) = event.response.id {
                    output.response_id = Some(id);
                }
            }
            DecodedResponseEvent::OutputItemAdded(event) => {
                let item = decode_output_item(event.item)?;
                create_slot(
                    event.output_index,
                    &item,
                    output,
                    sender,
                    &mut output_slots,
                    options.grammar_tool_input_properties,
                )?;
            }
            DecodedResponseEvent::ReasoningSummaryDelta(event)
            | DecodedResponseEvent::ReasoningTextDelta(event) => {
                let Some(OutputSlot::Thinking { content_index }) =
                    output_slots.get(&event.output_index)
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(block) = thinking_mut(output, content_index) {
                    block.thinking.push_str(&event.delta);
                }
                sender
                    .send(AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: event.delta,
                    })
                    .map_err(OpenAIResponsesError::display)?;
            }
            DecodedResponseEvent::ReasoningSummaryPartDone(event) => {
                let Some(OutputSlot::Thinking { content_index }) =
                    output_slots.get(&event.output_index)
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(block) = thinking_mut(output, content_index) {
                    block.thinking.push_str("\n\n");
                }
                sender
                    .send(AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta: "\n\n".to_owned(),
                    })
                    .map_err(OpenAIResponsesError::display)?;
            }
            DecodedResponseEvent::OutputTextDelta(event)
            | DecodedResponseEvent::RefusalDelta(event) => {
                let Some(OutputSlot::Text { content_index }) =
                    output_slots.get(&event.output_index)
                else {
                    continue;
                };
                let content_index = *content_index;
                if let Some(block) = text_mut(output, content_index) {
                    block.text.push_str(&event.delta);
                }
                sender
                    .send(AssistantMessageEvent::TextDelta {
                        content_index,
                        delta: event.delta,
                    })
                    .map_err(OpenAIResponsesError::display)?;
            }
            DecodedResponseEvent::FunctionArgumentsDelta(event) => {
                let Some(OutputSlot::FunctionCall {
                    content_index,
                    partial_json,
                }) = output_slots.get_mut(&event.output_index)
                else {
                    continue;
                };
                partial_json.push_str(&event.delta);
                if let Some(tool_call) = tool_call_mut(output, *content_index) {
                    tool_call.arguments = tool_arguments(partial_json);
                }
                sender
                    .send(AssistantMessageEvent::ToolCallDelta {
                        content_index: *content_index,
                        delta: event.delta,
                    })
                    .map_err(OpenAIResponsesError::display)?;
            }
            DecodedResponseEvent::FunctionArgumentsDone(event) => {
                let Some(OutputSlot::FunctionCall {
                    content_index,
                    partial_json,
                }) = output_slots.get_mut(&event.output_index)
                else {
                    continue;
                };
                let previous = partial_json.clone();
                partial_json.clone_from(&event.arguments);
                if let Some(tool_call) = tool_call_mut(output, *content_index) {
                    tool_call.arguments = tool_arguments(partial_json);
                }
                if let Some(delta) = event
                    .arguments
                    .strip_prefix(&previous)
                    .filter(|s| !s.is_empty())
                {
                    sender
                        .send(AssistantMessageEvent::ToolCallDelta {
                            content_index: *content_index,
                            delta: delta.to_owned(),
                        })
                        .map_err(OpenAIResponsesError::display)?;
                }
            }
            DecodedResponseEvent::CustomInputDelta(event) => {
                let Some(OutputSlot::CustomToolCall {
                    content_index,
                    property,
                    json_buffer,
                }) = output_slots.get_mut(&event.output_index)
                else {
                    continue;
                };
                let next = custom_tool_input(output, *content_index, property) + &event.delta;
                let delta = append_custom_tool_input(
                    output,
                    *content_index,
                    property,
                    json_buffer,
                    &next,
                    false,
                )?;
                push_tool_call_delta(sender, *content_index, delta)?;
            }
            DecodedResponseEvent::CustomInputDone(event) => {
                let Some(OutputSlot::CustomToolCall {
                    content_index,
                    property,
                    json_buffer,
                }) = output_slots.get_mut(&event.output_index)
                else {
                    continue;
                };
                let delta = append_custom_tool_input(
                    output,
                    *content_index,
                    property,
                    json_buffer,
                    &event.input,
                    true,
                )?;
                push_tool_call_delta(sender, *content_index, delta)?;
            }
            DecodedResponseEvent::OutputItemDone(event) => {
                let item = decode_output_item(event.item)?;
                apply_message_phase_stop_reason(&item, output);
                if !output_slots.contains_key(&event.output_index) {
                    create_slot(
                        event.output_index,
                        &item,
                        output,
                        sender,
                        &mut output_slots,
                        options.grammar_tool_input_properties,
                    )?;
                }
                let Some(slot) = output_slots.remove(&event.output_index) else {
                    continue;
                };
                match (item, slot) {
                    (
                        OutputItem::Reasoning { raw, item },
                        OutputSlot::Thinking { content_index },
                    ) => {
                        let summary = item
                            .summary
                            .iter()
                            .map(|summary| summary.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let content = item
                            .content
                            .iter()
                            .map(|content| content.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let signature = serde_json::to_string(&raw)
                            .expect("reasoning item value is serializable");
                        let final_content = {
                            let block = thinking_mut(output, content_index).ok_or_else(|| {
                                OpenAIResponsesError::new("missing thinking block")
                            })?;
                            if !summary.is_empty() {
                                block.thinking = summary;
                            } else if !content.is_empty() {
                                block.thinking = content;
                            }
                            block.thinking_signature = Some(signature.clone());
                            block.thinking.clone()
                        };
                        reasoning_blocks_by_id.insert(item.id, content_index);
                        sender
                            .send(AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content: final_content,
                                content_signature: Some(signature),
                                redacted: None,
                            })
                            .map_err(OpenAIResponsesError::display)?;
                    }
                    (OutputItem::Message(item), OutputSlot::Text { content_index }) => {
                        let content = message_item_text(&item);
                        let signature = encode_text_signature_v1(item.id, item.phase);
                        let block = text_mut(output, content_index)
                            .ok_or_else(|| OpenAIResponsesError::new("missing text block"))?;
                        block.text.clone_from(&content);
                        block.text_signature = Some(signature.clone());
                        sender
                            .send(AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                content_signature: Some(signature),
                            })
                            .map_err(OpenAIResponsesError::display)?;
                    }
                    (
                        OutputItem::FunctionCall(item),
                        OutputSlot::FunctionCall {
                            content_index,
                            partial_json,
                        },
                    ) => {
                        let arguments = if !item.arguments.is_empty() {
                            item.arguments
                        } else if !partial_json.is_empty() {
                            partial_json
                        } else {
                            "{}".to_owned()
                        };
                        let tool_call = tool_call_mut(output, content_index)
                            .ok_or_else(|| OpenAIResponsesError::new("missing tool-call block"))?;
                        tool_call.arguments = tool_arguments(&arguments);
                        if item.namespace.is_some() {
                            tool_call.namespace = item.namespace;
                        }
                        sender
                            .send(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call: tool_call.clone(),
                            })
                            .map_err(OpenAIResponsesError::display)?;
                    }
                    (
                        OutputItem::CustomToolCall(item),
                        OutputSlot::CustomToolCall {
                            content_index,
                            property,
                            mut json_buffer,
                        },
                    ) => {
                        let input = item
                            .input
                            .unwrap_or_else(|| custom_tool_input(output, content_index, &property));
                        let delta = append_custom_tool_input(
                            output,
                            content_index,
                            &property,
                            &mut json_buffer,
                            &input,
                            true,
                        )?;
                        push_tool_call_delta(sender, content_index, delta)?;
                        let tool_call = tool_call_mut(output, content_index)
                            .ok_or_else(|| OpenAIResponsesError::new("missing tool-call block"))?;
                        if item.namespace.is_some() {
                            tool_call.namespace = item.namespace;
                        }
                        sender
                            .send(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call: tool_call.clone(),
                            })
                            .map_err(OpenAIResponsesError::display)?;
                    }
                    (_, slot) => {
                        output_slots.insert(event.output_index, slot);
                    }
                }
            }
            DecodedResponseEvent::Completed(event) | DecodedResponseEvent::Incomplete(event) => {
                saw_terminal_response_event = true;
                finalize_response(
                    event.response,
                    output,
                    model,
                    &reasoning_blocks_by_id,
                    &options,
                )?;
            }
            DecodedResponseEvent::Error(event) => {
                return Err(OpenAIResponsesError::new(format!(
                    "Error Code {}: {}",
                    js_nullable_string(event.code),
                    js_nullable_string(event.message)
                )));
            }
            DecodedResponseEvent::Failed(event) => {
                output.raw_stop_reason = event.response.status;
                let message = event.response.error.map_or_else(
                    || {
                        event
                            .response
                            .incomplete_details
                            .and_then(|details| details.string_reason().map(str::to_owned))
                            .map_or_else(
                                || "Unknown error (no error details in response)".to_owned(),
                                |reason| format!("incomplete: {reason}"),
                            )
                    },
                    |error| {
                        format!(
                            "{}: {}",
                            error
                                .code
                                .filter(|code| !code.is_empty())
                                .unwrap_or_else(|| "unknown".to_owned()),
                            error
                                .message
                                .filter(|message| !message.is_empty())
                                .unwrap_or_else(|| "no message".to_owned())
                        )
                    },
                );
                return Err(OpenAIResponsesError::new(message));
            }
            DecodedResponseEvent::Unknown => {}
        }
    }
    if !saw_terminal_response_event {
        return Err(OpenAIResponsesError::new(
            "OpenAI Responses stream ended before a terminal response event",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, AssistantRole, ModelCost, ModelCostRates, StopReason, ToolResultMessage,
        ToolResultRole, UserMessage, UserRole,
    };
    use futures::StreamExt;
    use serde_json::json;
    use std::convert::Infallible;

    const COPILOT_RAW_TOOL_CALL_ID: &str = "call_4VnzVawQXPB9MgYib7CiQFEY|I9b95oN1wD/cHXKTw3PpRkL6KkCtzTJhUxMouMWYwHeTo2j3htzfSk7YPx2vifiIM4g3A8XXyOj8q4Bt6SLUG7gqY1E3ELkrkVQNHglRfUmWj84lqxJY+Puieb3VKyX0FB+83TUzn91cDMF/4gzt990IzqVrc+nIb9RRscRD070Du16q1glydVjWR0SBJsE6TbY/esOjFpqplogQqrajm1eI++f3eLi73R6q7hVusY0QbeFySVxABCjhN0lXB04caBe1rzHjYzul6MAXj7uq+0r17VLq+yrtyYhN12wkmFqHeqTyEei6EFPbMy24Nc+IbJlkP0OCg02W+gOnyBFcbi2ctvJFSOhSjt1CqBdqCnnhwUqXjbWiT0wh3DmLScRgTHmGkaI+oAcQQjfic65nxj+TnEkReA==";

    fn model(api: &str, provider: &str, id: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: api.into(),
            provider: provider.into(),
            base_url: "https://example.invalid/v1".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                rates: ModelCostRates::default(),
                tiers: None,
            },
            context_window: 400_000,
            max_tokens: 128_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn output(model: &Model) -> AssistantMessage {
        AssistantMessage::pending(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            1,
        )
    }

    fn user(text: &str, timestamp: i64) -> Message {
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(text.to_owned()),
            timestamp,
        }))
    }

    fn assistant(
        api: &str,
        provider: &str,
        model: &str,
        content: Vec<AssistantContent>,
        timestamp: i64,
    ) -> Message {
        let mut message = AssistantMessage::pending(api, provider, model, timestamp);
        message.role = AssistantRole::Assistant;
        message.content = content;
        message.stop_reason = StopReason::ToolUse;
        Message::Assistant(Box::new(message))
    }

    fn tool_result(
        id: &str,
        name: &str,
        content: Vec<ToolResultContent>,
        timestamp: i64,
    ) -> Message {
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            content,
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp,
        }))
    }

    fn allowed_providers() -> BTreeSet<String> {
        ["openai", "openai-codex", "opencode"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    async fn process(
        events: Vec<Value>,
        output: &mut AssistantMessage,
        model: &Model,
    ) -> (Result<(), OpenAIResponsesError>, Vec<AssistantMessageEvent>) {
        let mut input = futures::stream::iter(
            events
                .into_iter()
                .map(Ok::<Value, Infallible>)
                .collect::<Vec<_>>(),
        );
        let (sender, stream) = crate::event_stream::AssistantMessageEventStream::channel();
        let result = process_responses_stream(
            &mut input,
            output,
            &sender,
            model,
            OpenAIResponsesStreamOptions::default(),
        )
        .await;
        drop(sender);
        (result, stream.collect().await)
    }

    #[test]
    fn ports_empty_tool_result_placeholder() {
        let target = model("openai-responses", "openai", "gpt-4o-mini");
        let call = ToolCall::new(
            "tool-1",
            "bash",
            Map::from_iter([("command".to_owned(), Value::String("true".to_owned()))]),
        );
        let context = crate::types::Context {
            system_prompt: None,
            messages: vec![
                user("Run the command", 1),
                assistant(
                    "openai-responses",
                    "openai",
                    "gpt-4o-mini",
                    vec![AssistantContent::ToolCall(call)],
                    2,
                ),
                tool_result(
                    "tool-1",
                    "bash",
                    vec![ToolResultContent::Text(TextContent::new(""))],
                    3,
                ),
            ],
            tools: None,
        };
        let converted = convert_responses_messages(
            &target,
            &context,
            &allowed_providers(),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let output = converted
            .iter()
            .find_map(|item| match item {
                ResponseInputItem::ToolCallOutput(output) => Some(&output.output),
                _ => None,
            })
            .expect("function call output");
        assert_eq!(
            output,
            &ToolResultOutput::Text("(no tool output)".to_owned())
        );
    }

    #[test]
    fn response_tool_choice_preserves_null_and_discriminants() {
        let choice: ResponseToolChoice = serde_json::from_value(json!({
            "type":"mcp","server_label":"server","name":null
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(choice).unwrap(),
            json!({"type":"mcp","server_label":"server","name":null})
        );
        assert_eq!(
            serde_json::to_value(ResponseToolChoice::Hosted {
                kind: HostedToolChoiceType::WebSearchPreview2025_03_11,
            })
            .unwrap(),
            json!({"type":"web_search_preview_2025_03_11"})
        );
    }

    /// Pins reasoning replay and Azure terminal backfill from
    /// openai-responses-shared.ts:207-211, 510-534, and 675-693.
    #[tokio::test]
    async fn pins_reasoning_replay_shapes_and_terminal_backfill() {
        let target = model("openai-responses", "openai", "gpt-5.4");
        let mut thinking = ThinkingContent::new("summary");
        thinking.thinking_signature = Some(
            json!({
                "type":"reasoning","id":"rs_replay","summary":[],
                "encrypted_content":"encrypted-replay"
            })
            .to_string(),
        );
        let mut text = TextContent::new("answer");
        text.text_signature =
            Some(json!({"v":1,"id":"msg_replay","phase":"final_answer"}).to_string());
        let context = crate::types::Context {
            system_prompt: None,
            messages: vec![assistant(
                "openai-responses",
                "openai",
                "gpt-5.4",
                vec![
                    AssistantContent::Thinking(thinking),
                    AssistantContent::Text(text),
                ],
                1,
            )],
            tools: None,
        };
        let converted = convert_responses_messages(
            &target,
            &context,
            &allowed_providers(),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            converted.first(),
            Some(ResponseInputItem::Reasoning(value))
                if value["id"] == "rs_replay" && value["encrypted_content"] == "encrypted-replay"
        ));
        assert!(matches!(
            converted.get(1),
            Some(ResponseInputItem::OutputMessage(message))
                if message.id == "msg_replay" && message.phase == Some(TextSignaturePhase::FinalAnswer)
        ));

        let events = vec![
            json!({
                "type":"response.output_item.added","output_index":0,
                "item":{"type":"reasoning","id":"rs_backfill","summary":[]}
            }),
            json!({
                "type":"response.output_item.done","output_index":0,
                "item":{"type":"reasoning","id":"rs_backfill","summary":[{"text":"summary"}]}
            }),
            json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_backfill","status":"completed",
                    "output":[{
                        "type":"reasoning","id":"rs_backfill","summary":[],
                        "encrypted_content":"encrypted-terminal"
                    }]
                }
            }),
        ];
        let mut message = output(&target);
        let (result, _) = process(events, &mut message, &target).await;
        result.unwrap();
        let signature = match message.content.first() {
            Some(AssistantContent::Thinking(thinking)) => {
                thinking.thinking_signature.as_deref().expect("signature")
            }
            _ => panic!("expected thinking block"),
        };
        let signature: Value = serde_json::from_str(signature).unwrap();
        assert_eq!(signature["encrypted_content"], "encrypted-terminal");
    }

    /// Pins nullish-coalescing and explicit-null callback inputs from
    /// openai-responses-shared.ts:559-564.
    #[tokio::test]
    async fn service_tier_callbacks_preserve_null_presence() {
        let target = model("openai-responses", "openai", "gpt-5.4");
        let events = vec![json!({
            "type":"response.completed",
            "response":{"id":"resp_tier","status":"completed","service_tier":null}
        })];
        let mut input = futures::stream::iter(
            events
                .into_iter()
                .map(Ok::<Value, Infallible>)
                .collect::<Vec<_>>(),
        );
        let (sender, stream) = crate::event_stream::AssistantMessageEventStream::channel();
        let mut message = output(&target);
        let applied = std::sync::Mutex::new(None);
        let resolve = |response, request| {
            assert_eq!(response, Some(None));
            assert_eq!(request, Some(Some(ResponseServiceTier::Priority)));
            Some(None)
        };
        let apply = |_: &mut Usage, tier| {
            *applied.lock().expect("pricing observation") = Some(tier);
        };
        process_responses_stream(
            &mut input,
            &mut message,
            &sender,
            &target,
            OpenAIResponsesStreamOptions {
                service_tier: Some(Some(ResponseServiceTier::Priority)),
                grammar_tool_input_properties: None,
                resolve_service_tier: Some(&resolve),
                apply_service_tier_pricing: Some(&apply),
            },
        )
        .await
        .unwrap();
        drop(sender);
        let _ = stream.collect::<Vec<_>>().await;
        assert_eq!(*applied.lock().unwrap(), Some(Some(None)));
    }

    #[test]
    fn ports_foreign_tool_call_id_hashing() {
        let target = model("openai-codex-responses", "openai-codex", "gpt-5.5");
        let call = ToolCall::new(
            COPILOT_RAW_TOOL_CALL_ID,
            "edit",
            Map::from_iter([(
                "path".to_owned(),
                Value::String("src/styles/app.css".to_owned()),
            )]),
        );
        let context = crate::types::Context {
            system_prompt: Some("You are concise.".to_owned()),
            messages: vec![
                user("Use the tool.", 1),
                assistant(
                    "openai-responses",
                    "github-copilot",
                    "gpt-5.5",
                    vec![AssistantContent::ToolCall(call)],
                    2,
                ),
                tool_result(
                    COPILOT_RAW_TOOL_CALL_ID,
                    "edit",
                    vec![ToolResultContent::Text(TextContent::new("ok"))],
                    3,
                ),
            ],
            tools: None,
        };
        let converted = convert_responses_messages(
            &target,
            &context,
            &allowed_providers(),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let item_id = converted
            .iter()
            .find_map(|item| match item {
                ResponseInputItem::FunctionCall(call) => call.id.as_deref(),
                _ => None,
            })
            .expect("function-call item id");
        let raw_item_id = COPILOT_RAW_TOOL_CALL_ID
            .split('|')
            .nth(1)
            .expect("raw item id");
        assert_eq!(item_id, format!("fc_{}", short_hash(raw_item_id)));
        assert!(item_id.len() <= 64);
        assert!(
            item_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
    }

    #[test]
    fn ports_unique_fallback_message_ids() {
        let target = model("openai-codex-responses", "openai-codex", "gpt-5.5");
        let context = crate::types::Context {
            system_prompt: Some("You are concise.".to_owned()),
            messages: vec![
                user("hello", 1),
                assistant(
                    "anthropic-messages",
                    "anthropic",
                    "claude-opus-4-8",
                    vec![
                        AssistantContent::Thinking(ThinkingContent::new("private reasoning")),
                        AssistantContent::Text(TextContent::new("visible answer")),
                    ],
                    2,
                ),
            ],
            tools: None,
        };
        let converted = convert_responses_messages(
            &target,
            &context,
            &allowed_providers(),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let ids = converted
            .iter()
            .filter_map(|item| match item {
                ResponseInputItem::OutputMessage(message) => Some(message.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["msg_pi_1", "msg_pi_1_1"]);
    }

    /// Pins empty-user index advancement from openai-responses-shared.ts:206,349.
    #[test]
    fn empty_user_blocks_do_not_advance_fallback_message_id_index() {
        let target = model("openai-codex-responses", "openai-codex", "gpt-5.5");
        let context = crate::types::Context {
            system_prompt: None,
            messages: vec![
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Blocks(Vec::new()),
                    timestamp: 1,
                })),
                assistant(
                    "anthropic-messages",
                    "anthropic",
                    "claude-opus-4-8",
                    vec![AssistantContent::Text(TextContent::new("visible answer"))],
                    2,
                ),
            ],
            tools: None,
        };

        let converted = convert_responses_messages(
            &target,
            &context,
            &allowed_providers(),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let ids = converted
            .iter()
            .filter_map(|item| match item {
                ResponseInputItem::OutputMessage(message) => Some(message.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(ids, ["msg_pi_0"]);
    }

    fn function_call_events(namespace: Option<&str>) -> Vec<Value> {
        vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"function_call","id":"fc_test","call_id":"call_test","name":"lookup","arguments":""}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"function_call","id":"fc_test","call_id":"call_test","name":"lookup","arguments":"{\"value\":\"hello\"}","namespace":namespace}
            }),
            json!({"type":"response.completed","response":{"id":"resp_test","status":"completed"}}),
        ]
    }

    fn custom_call_events() -> Vec<Value> {
        vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"custom_tool_call","id":"ctc_test","call_id":"call_test","name":"query","input":""}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"custom_tool_call","id":"ctc_test","call_id":"call_test","name":"query","input":"hello","namespace":"dynamic_tools"}
            }),
            json!({"type":"response.completed","response":{"id":"resp_test","status":"completed"}}),
        ]
    }

    fn first_tool_call(output: &AssistantMessage) -> &ToolCall {
        match output.content.first() {
            Some(AssistantContent::ToolCall(call)) => call,
            _ => panic!("expected tool call"),
        }
    }

    #[tokio::test]
    async fn ports_function_namespace_round_trip() {
        let target = model("openai-responses", "openai", "gpt-5.4");
        let mut message = output(&target);
        let (result, _) = process(
            function_call_events(Some("dynamic_tools")),
            &mut message,
            &target,
        )
        .await;
        result.unwrap();
        assert_eq!(
            first_tool_call(&message),
            &ToolCall {
                kind: Default::default(),
                id: "call_test|fc_test".to_owned(),
                name: "lookup".to_owned(),
                arguments: Map::from_iter([(
                    "value".to_owned(),
                    Value::String("hello".to_owned()),
                )]),
                thought_signature: None,
                namespace: Some("dynamic_tools".to_owned()),
            }
        );
        let replay = convert_responses_messages(
            &target,
            &crate::types::Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(message))],
                tools: None,
            },
            &BTreeSet::from(["openai".to_owned()]),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let replay = replay
            .iter()
            .find_map(|item| match item {
                ResponseInputItem::FunctionCall(call) => Some(call),
                _ => None,
            })
            .expect("replayed function call");
        assert_eq!(replay.namespace.as_deref(), Some("dynamic_tools"));
        assert_eq!(replay.arguments, "{\"value\":\"hello\"}");
    }

    #[tokio::test]
    async fn ports_custom_tool_namespace_round_trip() {
        let target = model("openai-responses", "openai", "gpt-5.4");
        let grammar = BTreeMap::from([("query".to_owned(), "input".to_owned())]);
        let mut message = output(&target);
        let mut input = futures::stream::iter(
            custom_call_events()
                .into_iter()
                .map(Ok::<Value, Infallible>)
                .collect::<Vec<_>>(),
        );
        let (sender, stream) = crate::event_stream::AssistantMessageEventStream::channel();
        process_responses_stream(
            &mut input,
            &mut message,
            &sender,
            &target,
            OpenAIResponsesStreamOptions {
                grammar_tool_input_properties: Some(&grammar),
                ..OpenAIResponsesStreamOptions::default()
            },
        )
        .await
        .unwrap();
        drop(sender);
        let _ = stream.collect::<Vec<_>>().await;
        let call = first_tool_call(&message);
        assert_eq!(call.namespace.as_deref(), Some("dynamic_tools"));
        assert_eq!(call.arguments.get("input"), Some(&json!("hello")));

        let replay = convert_responses_messages(
            &target,
            &crate::types::Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(message))],
                tools: None,
            },
            &BTreeSet::from(["openai".to_owned()]),
            ConvertResponsesMessagesOptions {
                grammar_tool_input_properties: Some(&grammar),
                ..ConvertResponsesMessagesOptions::default()
            },
        )
        .unwrap();
        let replay = replay
            .iter()
            .find_map(|item| match item {
                ResponseInputItem::CustomToolCall(call) => Some(call),
                _ => None,
            })
            .expect("replayed custom tool call");
        assert_eq!(replay.namespace.as_deref(), Some("dynamic_tools"));
        assert_eq!(replay.input, "hello");
    }

    #[test]
    fn ports_namespace_drop_for_unreplayable_targets() {
        let source_model = model("openai-responses", "openai", "gpt-5.4");
        let mut source = output(&source_model);
        let mut function = ToolCall::new(
            "call_function|fc_test",
            "lookup",
            Map::from_iter([("value".to_owned(), json!("hello"))]),
        );
        function.namespace = Some("dynamic_tools".to_owned());
        let mut custom = ToolCall::new(
            "call_custom|ctc_test",
            "query",
            Map::from_iter([("input".to_owned(), json!("hello"))]),
        );
        custom.namespace = Some("dynamic_tools".to_owned());
        source.content = vec![
            AssistantContent::ToolCall(function),
            AssistantContent::ToolCall(custom),
        ];
        let targets = [
            model("openai-responses", "openai", "gpt-5.2"),
            model("openai-responses", "azure-openai-responses", "gpt-5.4"),
            model(
                "openai-codex-responses",
                "openai-codex",
                "gpt-5.3-codex-spark",
            ),
        ];
        let grammar = BTreeMap::from([("query".to_owned(), "input".to_owned())]);
        for target in targets {
            let replay = convert_responses_messages(
                &target,
                &crate::types::Context {
                    system_prompt: None,
                    messages: vec![Message::Assistant(Box::new(source.clone()))],
                    tools: None,
                },
                &BTreeSet::from(["openai".to_owned()]),
                ConvertResponsesMessagesOptions {
                    grammar_tool_input_properties: Some(&grammar),
                    ..ConvertResponsesMessagesOptions::default()
                },
            )
            .unwrap();
            for item in replay {
                match item {
                    ResponseInputItem::FunctionCall(call) => assert_eq!(call.namespace, None),
                    ResponseInputItem::CustomToolCall(call) => assert_eq!(call.namespace, None),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn ports_absent_namespace_for_ordinary_function_calls() {
        let target = model("openai-responses", "openai", "gpt-5.4");
        let mut source = output(&target);
        source
            .content
            .push(AssistantContent::ToolCall(ToolCall::new(
                "call_test|fc_test",
                "lookup",
                Map::from_iter([("value".to_owned(), json!("hello"))]),
            )));
        let replay = convert_responses_messages(
            &target,
            &crate::types::Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(source))],
                tools: None,
            },
            &BTreeSet::from(["openai".to_owned()]),
            ConvertResponsesMessagesOptions::default(),
        )
        .unwrap();
        let call = replay
            .iter()
            .find_map(|item| match item {
                ResponseInputItem::FunctionCall(call) => Some(call),
                _ => None,
            })
            .expect("function call");
        assert_eq!(call.namespace, None);
    }

    #[tokio::test]
    async fn ports_partial_json_cleanup() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let arguments = "{\"path\":\"README.md\",\"content\":\"updated\"}";
        let events = vec![
            json!({
                "type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","id":"fc_test","call_id":"call_test","name":"edit","arguments":""}
            }),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"README.md\""}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":",\"content\":\"updated\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":arguments}),
            json!({
                "type":"response.output_item.done","output_index":0,
                "item":{"type":"function_call","id":"fc_test","call_id":"call_test","name":"edit","arguments":arguments}
            }),
            json!({"type":"response.completed","response":{"id":"resp_test","status":"completed"}}),
        ];
        let mut message = output(&target);
        let (result, emitted) = process(events, &mut message, &target).await;
        result.unwrap();
        let persisted = first_tool_call(&message);
        assert_eq!(
            persisted.arguments,
            Map::from_iter([
                ("path".to_owned(), json!("README.md")),
                ("content".to_owned(), json!("updated")),
            ])
        );
        let ended = emitted
            .iter()
            .find_map(|event| match event {
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("toolcall_end event");
        assert_eq!(ended, persisted);
        let serialized = serde_json::to_value(ended).unwrap();
        assert!(serialized.get("partialJson").is_none());
    }

    fn early_eof_events() -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":{"id":"resp_early_eof"}}),
            json!({
                "type":"response.output_item.added","output_index":0,
                "item":{"type":"reasoning","id":"rs_early_eof","summary":[]}
            }),
            json!({
                "type":"response.reasoning_text.delta","output_index":0,
                "delta":"partial reasoning before the stream ends"
            }),
        ]
    }

    fn incomplete_event(reason: &str) -> Value {
        json!({
            "type":"response.incomplete",
            "response":{
                "id":"resp_incomplete","status":"incomplete",
                "incomplete_details":{"reason":reason},
                "usage":{
                    "input_tokens":30,"output_tokens":12,"total_tokens":42,
                    "input_tokens_details":{"cached_tokens":5}
                }
            }
        })
    }

    #[tokio::test]
    async fn ports_rejection_of_early_eof() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let (result, _) = process(early_eof_events(), &mut message, &target).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "OpenAI Responses stream ended before a terminal response event"
        );
    }

    #[test]
    fn ports_message_phase_tracking() {
        for (added, done, expected) in [
            (
                "commentary",
                "commentary",
                [StopReason::Pending, StopReason::Pending],
            ),
            (
                "final_answer",
                "final_answer",
                [StopReason::Stop, StopReason::Stop],
            ),
            (
                "commentary",
                "final_answer",
                [StopReason::Pending, StopReason::Stop],
            ),
        ] {
            let target = model("openai-responses", "openai", "gpt-5-mini");
            let mut message = output(&target);
            let added = decode_output_item(json!({
                "type":"message","id":"msg_phase","content":[],"phase":added
            }))
            .unwrap();
            apply_message_phase_stop_reason(&added, &mut message);
            assert_eq!(message.stop_reason, expected[0]);
            let done = decode_output_item(json!({
                "type":"message","id":"msg_phase","content":[],"phase":done
            }))
            .unwrap();
            apply_message_phase_stop_reason(&done, &mut message);
            assert_eq!(message.stop_reason, expected[1]);
        }
    }

    #[tokio::test]
    async fn ports_incomplete_terminal_overrides_final_answer_phase() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let events = vec![
            json!({
                "type":"response.output_item.added","output_index":0,
                "item":{"type":"message","id":"msg_phase","content":[],"phase":"final_answer"}
            }),
            json!({
                "type":"response.output_item.done","output_index":0,
                "item":{"type":"message","id":"msg_phase","content":[{"type":"output_text","text":"answer"}],"phase":"final_answer"}
            }),
            incomplete_event("max_output_tokens"),
        ];
        let (result, _) = process(events, &mut message, &target).await;
        result.unwrap();
        assert_eq!(message.stop_reason, StopReason::Length);
    }

    #[tokio::test]
    async fn ports_completed_terminal_usage() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let events = vec![json!({
            "type":"response.completed",
            "response":{
                "id":"resp_completed","status":"completed",
                "usage":{
                    "input_tokens":20,"output_tokens":7,"total_tokens":27,
                    "input_tokens_details":{"cached_tokens":2,"cache_write_tokens":3}
                }
            }
        })];
        let (result, _) = process(events, &mut message, &target).await;
        result.unwrap();
        assert_eq!(message.response_id.as_deref(), Some("resp_completed"));
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.raw_stop_reason.as_deref(), Some("completed"));
        assert_eq!(message.usage.input, 15);
        assert_eq!(message.usage.output, 7);
        assert_eq!(message.usage.cache_read, 2);
        assert_eq!(message.usage.cache_write, 3);
        assert_eq!(message.usage.total_tokens, 27);
    }

    #[tokio::test]
    async fn ports_incomplete_terminal_as_length() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let (result, _) = process(
            vec![incomplete_event("max_output_tokens")],
            &mut message,
            &target,
        )
        .await;
        result.unwrap();
        assert_eq!(message.response_id.as_deref(), Some("resp_incomplete"));
        assert_eq!(message.stop_reason, StopReason::Length);
        assert_eq!(
            message.raw_stop_reason.as_deref(),
            Some("incomplete.max_output_tokens")
        );
        assert_eq!(message.usage.input, 25);
        assert_eq!(message.usage.output, 12);
        assert_eq!(message.usage.cache_read, 5);
        assert_eq!(message.usage.cache_write, 0);
        assert_eq!(message.usage.total_tokens, 42);
    }

    #[tokio::test]
    async fn ports_content_filtered_incomplete_as_error() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let (result, _) = process(
            vec![incomplete_event("content_filter")],
            &mut message,
            &target,
        )
        .await;
        result.unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.raw_stop_reason.as_deref(),
            Some("incomplete.content_filter")
        );
        assert_eq!(
            message.error_message.as_deref(),
            Some("Response incomplete: content_filter")
        );
    }

    #[tokio::test]
    async fn ports_unknown_incomplete_reason_as_error() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let (result, _) = process(
            vec![incomplete_event("max_time_limit")],
            &mut message,
            &target,
        )
        .await;
        result.unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.raw_stop_reason.as_deref(),
            Some("incomplete.max_time_limit")
        );
        assert_eq!(
            message.error_message.as_deref(),
            Some("Response incomplete: max_time_limit")
        );
    }

    #[tokio::test]
    async fn ports_failed_terminal_provider_error() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let events = vec![json!({
            "type":"response.failed",
            "response":{
                "id":"resp_failed","status":"failed",
                "error":{"code":"server_error","message":"boom"}
            }
        })];
        let (result, _) = process(events, &mut message, &target).await;
        assert_eq!(result.unwrap_err().to_string(), "server_error: boom");
        assert_eq!(message.raw_stop_reason.as_deref(), Some("failed"));
    }

    #[tokio::test]
    async fn pins_unknown_event_forward_compatibility() {
        let target = model("openai-responses", "openai", "gpt-5-mini");
        let mut message = output(&target);
        let events = vec![
            json!({"type":"response.future_event","unexpected":{"shape":true}}),
            json!({"type":"response.completed","response":{"id":"resp_test","status":"completed"}}),
        ];
        let (result, emitted) = process(events, &mut message, &target).await;
        result.unwrap();
        assert!(emitted.is_empty());
        assert_eq!(message.stop_reason, StopReason::Stop);
    }

    #[test]
    fn pins_live_tool_result_image_test_conversion_hermetically() {
        for (api, provider, id) in [
            ("openai-responses", "openai", "gpt-5-mini"),
            (
                "azure-openai-responses",
                "azure-openai-responses",
                "gpt-4o-mini",
            ),
            ("openai-responses", "github-copilot", "gpt-5-mini"),
            ("openai-codex-responses", "openai-codex", "gpt-5.5"),
        ] {
            let mut target = model(api, provider, id);
            target.input.push(ModelInput::Image);
            let call = ToolCall::new(
                "call_test|fc_test",
                "get_circle_with_description",
                Map::new(),
            );
            let context = crate::types::Context {
                system_prompt: None,
                messages: vec![
                    user("Call the image tool", 1),
                    assistant(api, provider, id, vec![AssistantContent::ToolCall(call)], 2),
                    tool_result(
                        "call_test|fc_test",
                        "get_circle_with_description",
                        vec![
                            ToolResultContent::Text(TextContent::new(
                                "A red circle with a diameter of 100 pixels.",
                            )),
                            ToolResultContent::Image(ImageContent::new(
                                "iVBORw0KGgo=",
                                "image/png",
                            )),
                        ],
                        3,
                    ),
                ],
                tools: None,
            };
            let converted = convert_responses_messages(
                &target,
                &context,
                &allowed_providers(),
                ConvertResponsesMessagesOptions::default(),
            )
            .unwrap();
            let output_index = converted
                .iter()
                .position(|item| matches!(item, ResponseInputItem::ToolCallOutput(_)))
                .expect("function-call output");
            let output = match &converted[output_index] {
                ResponseInputItem::ToolCallOutput(output) => &output.output,
                _ => unreachable!(),
            };
            let ToolResultOutput::Content(content) = output else {
                panic!("expected content-array output")
            };
            assert!(
                matches!(content.first(), Some(ResponseInputContent::InputText { text }) if text.contains("red circle"))
            );
            assert!(
                matches!(content.get(1), Some(ResponseInputContent::InputImage { image_url, .. }) if image_url.starts_with("data:image/png;base64,"))
            );
            assert!(
                !converted[output_index + 1..]
                    .iter()
                    .any(|item| matches!(item, ResponseInputItem::User(_)))
            );
        }
    }
}
