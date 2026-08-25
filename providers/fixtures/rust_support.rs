//! Shared parser for the captured §10.8 family corpora.

use pi_ai::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

pub fn family_cases(manifest_dir: &str, family: &str) -> Vec<PathBuf> {
    let root = Path::new(manifest_dir).join("../fixtures").join(family);
    let mut cases = fs::read_dir(root)
        .expect("fixture family")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    cases.sort();
    assert_eq!(cases.len(), 28, "captured {family} fixture count changed");
    cases
}

pub fn canonical(case: &Path) -> Value {
    serde_json::from_slice(&fs::read(case.join("canonical.json")).expect("canonical fixture"))
        .expect("canonical fixture JSON")
}

pub fn common_model(value: &Value) -> CommonModelDescriptor {
    let mut headers = HeaderMapSpec::new();
    for (name, value) in value["headers"].as_object().into_iter().flatten() {
        headers.insert(name.clone(), value.as_str().map(str::to_owned));
    }
    CommonModelDescriptor {
        model_ref: ModelRef::new(
            value["provider"].as_str().expect("fixture provider"),
            value["id"].as_str().expect("fixture model"),
        ),
        display_name: value["name"].as_str().expect("fixture name").into(),
        base_url: Url::parse("http://127.0.0.1:43123/v1").unwrap(),
        modalities: ModalityCapabilities {
            input: value["input"]
                .as_array()
                .into_iter()
                .flatten()
                .map(
                    |modality| match modality.as_str().expect("input modality") {
                        "text" => Modality::Text,
                        "image" => Modality::Image,
                        other => panic!("unknown fixture modality {other}"),
                    },
                )
                .collect(),
            output: BTreeSet::from([Modality::Text]),
        },
        limits: ModelLimits {
            context_window: value["contextWindow"].as_u64().expect("context window"),
            max_output_tokens: value["maxTokens"].as_u64().expect("max tokens") as u32,
        },
        pricing: ModelPricing {
            default: TokenPriceRates::default(),
            request_wide_tiers: Vec::new(),
            cache_write_retention: CacheWriteRetentionPricing::default(),
        },
        reasoning: value["reasoning"].as_bool().unwrap_or(false),
        headers,
    }
}

pub fn context(value: &Value, model: &CommonModelDescriptor, api: &str) -> Context {
    Context {
        schema_version: 1,
        system_prompt: value
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        messages: value["messages"]
            .as_array()
            .expect("fixture messages")
            .iter()
            .enumerate()
            .map(|(index, message)| message_from_value(message, index, model, api))
            .collect(),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(tool_from_value)
            .collect(),
    }
}

pub fn append_messages(
    context: &mut Context,
    values: &Value,
    model: &CommonModelDescriptor,
    api: &str,
) {
    let first = context.messages.len();
    for (offset, value) in values.as_array().into_iter().flatten().enumerate() {
        context
            .messages
            .push(message_from_value(value, first + offset, model, api));
    }
}

pub fn message_from_value(
    value: &Value,
    index: usize,
    model: &CommonModelDescriptor,
    api: &str,
) -> Message {
    let id = MessageId::new(format!("fixture-message-{index}"));
    let timestamp =
        Timestamp::from_unix_millis(value["timestamp"].as_i64().unwrap_or(1_700_000_000_000));
    match value["role"].as_str().expect("fixture role") {
        "user" => Message::User(UserMessage {
            id,
            content: content_from_value(&value["content"], index).0,
            timestamp,
        }),
        "assistant" => {
            let provider = value["provider"]
                .as_str()
                .unwrap_or(model.model_ref.provider.as_str());
            let source_api = value["api"].as_str().unwrap_or(api);
            let requested_model = value["model"]
                .as_str()
                .unwrap_or(model.model_ref.model.as_str());
            let (content, signatures) = content_from_value(&value["content"], 10_000 + index);
            let mut replay = ReplayEnvelope::new(ReplayScope::new(
                provider,
                source_api,
                requested_model,
                requested_model,
            ));
            for (ordinal, signature) in signatures.into_iter().enumerate() {
                replay.items.push(ReplayItem {
                    id: ReplayItemId::new(format!("fixture-signature-{index}-{ordinal}")),
                    ordinal: ordinal as u32,
                    target: signature.target,
                    kind: ReplayKind::new(GOOGLE_THOUGHT_SIGNATURE_KIND),
                    applicability: ReplayApplicability::ExactProviderApiModel,
                    completeness: ReplayCompleteness::Complete,
                    payload: OpaquePayload::Utf8(signature.value),
                });
            }
            Message::Assistant(AssistantMessage {
                id,
                provider: provider.into(),
                api: source_api.into(),
                requested_model: requested_model.into(),
                response_model: value
                    .get("responseModel")
                    .and_then(Value::as_str)
                    .map(Into::into),
                response_id: value
                    .get("responseId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                deferred: None,
                end_turn: value.get("endTurn").and_then(Value::as_bool),
                diagnostics: Vec::new(),
                content,
                replay,
                usage: usage(&value["usage"]),
                cost: None,
                finish: AssistantFinish {
                    reason: finish_reason(value["stopReason"].as_str().unwrap_or("stop")),
                    raw_provider_reason: value
                        .get("rawStopReason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    error: value
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .map(|message| PublicError {
                            code: "fixture_error".into(),
                            message: message.into(),
                            retryable: false,
                            provider_code: None,
                            status: None,
                            request_id: None,
                        }),
                },
                timestamp,
            })
        }
        "toolResult" => Message::ToolResult(ToolResultMessage {
            id,
            tool_call_id: ToolCallId::new(
                value["toolCallId"].as_str().expect("fixture tool call ID"),
            ),
            tool_name: value["toolName"].as_str().unwrap_or_default().into(),
            content: value["content"]
                .as_array()
                .expect("fixture tool result content")
                .iter()
                .enumerate()
                .map(|(block_index, block)| {
                    let id =
                        ContentBlockId::new(format!("fixture-tool-block-{index}-{block_index}"));
                    match block["type"].as_str().expect("tool result type") {
                        "text" => ToolResultContent::Text {
                            id,
                            text: block["text"].as_str().expect("tool result text").into(),
                        },
                        "image" => ToolResultContent::Image {
                            id,
                            data: block["data"].as_str().expect("tool image data").into(),
                            mime_type: block["mimeType"].as_str().expect("tool image MIME").into(),
                        },
                        other => panic!("unknown tool result type {other}"),
                    }
                })
                .collect(),
            details: None,
            usage: value.get("usage").map(usage),
            added_tool_names: value["addedToolNames"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|name| name.as_str().expect("added tool name").to_owned())
                .collect(),
            is_error: value["isError"].as_bool().unwrap_or(false),
            timestamp,
        }),
        other => panic!("unknown fixture role {other}"),
    }
}

struct Signature {
    target: ReplayTarget,
    value: String,
}

fn content_from_value(value: &Value, message_index: usize) -> (Vec<ContentBlock>, Vec<Signature>) {
    if let Some(text) = value.as_str() {
        return (
            vec![ContentBlock::Text {
                id: ContentBlockId::new(format!("fixture-block-{message_index}-0")),
                text: text.into(),
            }],
            Vec::new(),
        );
    }
    let mut content = Vec::new();
    let mut signatures = Vec::new();
    for (block_index, block) in value
        .as_array()
        .expect("fixture content")
        .iter()
        .enumerate()
    {
        let id = ContentBlockId::new(format!("fixture-block-{message_index}-{block_index}"));
        let content_block = match block["type"].as_str().expect("fixture content type") {
            "text" => ContentBlock::Text {
                id: id.clone(),
                text: block["text"].as_str().expect("fixture text").into(),
            },
            "image" => ContentBlock::Image {
                id: id.clone(),
                data: block["data"].as_str().expect("fixture image").into(),
                mime_type: block["mimeType"]
                    .as_str()
                    .expect("fixture image MIME")
                    .into(),
            },
            "thinking" => ContentBlock::Thinking {
                id: id.clone(),
                text: block["thinking"].as_str().expect("fixture thinking").into(),
                redacted: block["redacted"].as_bool().unwrap_or(false),
                replay_item: None,
            },
            "toolCall" => ContentBlock::ToolCall {
                id: id.clone(),
                call: ToolCall {
                    id: ToolCallId::new(block["id"].as_str().expect("fixture tool call")),
                    name: block["name"].as_str().expect("fixture tool name").into(),
                    arguments: block["arguments"].clone(),
                },
            },
            other => panic!("unknown fixture content type {other}"),
        };
        let signature = block
            .get("textSignature")
            .or_else(|| block.get("thinkingSignature"))
            .or_else(|| block.get("thoughtSignature"))
            .and_then(Value::as_str);
        if let Some(value) = signature {
            let target = match &content_block {
                ContentBlock::ToolCall { call, .. } => ReplayTarget::ToolCall(call.id.clone()),
                _ => ReplayTarget::ContentBlock(id),
            };
            signatures.push(Signature {
                target,
                value: value.to_owned(),
            });
        }
        content.push(content_block);
    }
    (content, signatures)
}

fn tool_from_value(value: &Value) -> ToolSpec {
    ToolSpec {
        schema_version: 1,
        name: value["name"].as_str().expect("fixture tool name").into(),
        description: value["description"]
            .as_str()
            .expect("fixture tool description")
            .into(),
        parameters: value["parameters"].clone(),
        constrained_sampling: value
            .get("constrainedSampling")
            .map(|value| serde_json::from_value(value.clone()).expect("constrained sampling")),
    }
}

pub fn simple_options(value: &Value) -> SimpleGenerationOptions {
    let mut headers = HeaderMapSpec::new();
    for (name, value) in value["headers"].as_object().into_iter().flatten() {
        headers.insert(name.clone(), value.as_str().map(str::to_owned));
    }
    SimpleGenerationOptions {
        max_output_tokens: value
            .get("maxTokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_str)
            .map(|reasoning| {
                serde_json::from_value(Value::String(reasoning.into())).expect("fixture reasoning")
            }),
        cache_retention: value
            .get("cacheRetention")
            .and_then(Value::as_str)
            .map(cache_retention),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        headers,
        max_retries: value
            .get("maxRetries")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        timeout_ms: value.get("timeoutMs").and_then(Value::as_u64),
        ..Default::default()
    }
}

pub fn cache_retention(value: &str) -> CacheRetention {
    match value {
        "none" => CacheRetention::None,
        "short" => CacheRetention::Short,
        "long" => CacheRetention::Long,
        other => panic!("unknown fixture retention {other}"),
    }
}

pub fn usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value["input"].as_u64().unwrap_or(0),
        output_tokens: value["output"].as_u64().unwrap_or(0),
        reasoning_tokens: value.get("reasoning").and_then(Value::as_u64),
        cache_read_tokens: value.get("cacheRead").and_then(Value::as_u64),
        cache_write_tokens: value.get("cacheWrite").and_then(Value::as_u64),
        cache_write_one_hour_tokens: value.get("cacheWrite1h").and_then(Value::as_u64),
        total_tokens: value.get("totalTokens").and_then(Value::as_u64),
        source: UsageSource::Unknown,
    }
}

fn finish_reason(value: &str) -> AssistantFinishReason {
    match value {
        "stop" => AssistantFinishReason::Stop,
        "length" => AssistantFinishReason::Length,
        "toolUse" => AssistantFinishReason::ToolUse,
        "deferred" => AssistantFinishReason::Deferred,
        "error" => AssistantFinishReason::Error,
        "aborted" => AssistantFinishReason::Aborted,
        other => panic!("unknown fixture finish reason {other}"),
    }
}
