use agentprism_ai::{
    ApiFamily, ApiModelConfig, AssistantFinish, AssistantFinishReason, AssistantMessage,
    AttemptFailure, CommonModelDescriptor, ConstrainedSampling, ConstrainedSamplingConfig,
    ContentBlock, ContentBlockId, Context, DefaultRetryClassifier, EncodeContext,
    ErasedApiOptionsPatch, GOOGLE_THOUGHT_SIGNATURE_KIND, GoogleCompat, GoogleGenerativeAi,
    GoogleHandoff, GoogleModelConfig, GoogleOptions, GoogleSimplePatch, GoogleThinkingLevel,
    GoogleThinkingOptions, GoogleToolChoice, GoogleVertex, GoogleVertexOptions,
    JsonSchemaStrictMode, LevelSupport, LocalDefaultRetryClassifier, Message, MessageId, Modality,
    ModalityCapabilities, ModelDescriptor, ModelId, ModelLimits, ModelPricing, ModelRef,
    ModelRequest, Models, MoneyRate, OpaquePayload, OrderedJsonObject, OrderedJsonValue,
    OrderedJsonWriter, ProviderId, ReasoningLevel, ReplayApplicability, ReplayCompleteness,
    ReplayEnvelope, ReplayItem, ReplayItemId, ReplayKind, ReplayScope, ReplayTarget,
    RetryClassifier, RetryDecision, RetryJitter, RetryPolicy, SecretString,
    SimpleGenerationOptions, SimpleLoweringContext, ThinkingBudgets, ThinkingLevelMap, Timestamp,
    TokenPriceRates, ToolCall, ToolCallId, ToolResultContent, ToolResultMessage, ToolSpec,
    TypedModelDescriptor, Usage, UsageSource, convert_google_tools, estimate_context_tokens,
    requires_google_tool_call_id, supports_google_strict_tool_sampling,
    transform_context_for_model,
};
use agentprism_google::{
    GoogleDecodeContext, decode_google_sse, google_models, google_provider, google_user_agent,
    local_google_provider,
};
use agentprism_google_vertex::{
    google_vertex_models, google_vertex_provider, local_google_vertex_provider,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

const FIXTURE_TIMESTAMP: i64 = 1_700_000_000_000;
const SERVICE_ACCOUNT_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIICdwIBADANBgkqhkiG9w0BAQEFAASCAmEwggJdAgEAAoGBAOFe1bZS7EXF8XfM
wDAk5LZrZzUIwhL9s0Vp+6AHtiFG2jZjzUSP3L0ejlgKAwzXzqTbmB0Nhs0FXdvF
/Im1NjOCz9DEPN1FCPY8vi5WxpEsH36HE3n3KLLZnb0Sb65AOn4NDNdDJzw5jbiT
zIBGpVVm7rHiZ+rZpDFBuEB7pZ8lAgMBAAECgYEA4D4VVUGzGEW5WqNfA0hiTeQW
IB3jxTOsAcBPf07M+NVf4EhzgOnIEGDr70ue91NvqHdbJmVEIJFbb4bTHU4yazZ7
9tV35Cm9daqZOPBtyAyy3X7MAusawC8ZPCUFSixM7F/QWec+6H/QoEAs30iwfNSB
0kPzs0mExa1QpFdum6ECQQD1fV0Rc0x9oumU30Co22PHehxbccNwnhxq7EKzZYVH
iZS27TzfgExKXSlxbhXv0HyI7F/sAks/KfD0RxA+DKxNAkEA6wT1ZQWBtoXe5VyV
pdIc5hjSbl1ynK0xWLj+utoAQxncuEU+wQFes/TFs0RgntsTTahILiDYgvMjhZeX
o5lKOQJAcZpqD0FEDH/viC0oRvv/2Lfxl3+16c/BZtmepFY+rzRD1cNDgEpnA6LJ
IuzGygu5FcQNP7JwD/Lgxqp8IbrLoQJABIaN6yoV+1vMlQIZZ54KLGwh8TofcODs
6FZ3oUV9Z81hsLK0qKbMGg8Gl5MjgSuazY4GBc1gHfVso6/tnZrgEQJBAK/tJqD9
0F/zlQ+Y9HQRAxvS3RcyJenNbMcZJGNOxSfpw0xgradYMtSc+REUsApjoxVDc1Nr
TLSf3E73EtEtajg=
-----END PRIVATE KEY-----"#;

fn fixture_root(family: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(family)
}

fn case_names(family: &str) -> Vec<String> {
    let mut names = fs::read_dir(fixture_root(family))
        .expect("fixture root")
        .map(|entry| {
            entry
                .expect("fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn read_value(family: &str, case: &str, file: &str) -> Value {
    serde_json::from_slice(
        &fs::read(fixture_root(family).join(case).join(file)).expect("fixture file"),
    )
    .expect("fixture JSON")
}

fn read_bytes(family: &str, case: &str, file: &str) -> Vec<u8> {
    fs::read(fixture_root(family).join(case).join(file)).expect("fixture bytes")
}

fn parse_fixture(
    family: &str,
    case: &str,
) -> (Value, ModelDescriptor, Context, SimpleGenerationOptions) {
    let fixture = read_value(family, case, "canonical.json");
    let model = parse_model(&fixture["model"]);
    let context = parse_context(&fixture["context"], &model);
    let options = parse_simple(&fixture["options"]);
    (fixture, model, context, options)
}

fn parse_model(value: &Value) -> ModelDescriptor {
    let provider = value["provider"].as_str().expect("provider");
    let model_id = value["id"].as_str().expect("model ID");
    let family = value["api"].as_str().expect("API family");
    let input = value["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| match value {
            "text" => Some(Modality::Text),
            "image" => Some(Modality::Image),
            "audio" => Some(Modality::Audio),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let config = GoogleModelConfig {
        thinking_levels: parse_thinking_levels(&value["thinkingLevelMap"]),
    };
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, model_id),
            display_name: value["name"].as_str().unwrap_or(model_id).to_owned(),
            base_url: Url::parse("http://127.0.0.1:1/v1").expect("fixture URL"),
            modalities: ModalityCapabilities {
                input,
                output: [Modality::Text].into_iter().collect(),
            },
            limits: ModelLimits {
                context_window: value["contextWindow"].as_u64().expect("context window"),
                max_output_tokens: u32::try_from(value["maxTokens"].as_u64().expect("max tokens"))
                    .expect("u32 max tokens"),
            },
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: Default::default(),
            },
            reasoning: value["reasoning"].as_bool().unwrap_or(false),
            headers: Default::default(),
        },
        api: if family == GoogleGenerativeAi::API_ID {
            ApiModelConfig::GoogleGenerativeAi(config)
        } else {
            ApiModelConfig::GoogleVertex(config)
        },
        extensions: Default::default(),
    }
}

fn parse_thinking_levels(value: &Value) -> ThinkingLevelMap<String> {
    let parse = |name: &str| {
        value.get(name).map(|value| {
            value.as_str().map_or(LevelSupport::Unsupported, |value| {
                LevelSupport::Value(value.to_owned())
            })
        })
    };
    ThinkingLevelMap {
        off: parse("off"),
        minimal: parse("minimal"),
        low: parse("low"),
        medium: parse("medium"),
        high: parse("high"),
        xhigh: parse("xhigh"),
        max: parse("max"),
    }
}

fn parse_simple(value: &Value) -> SimpleGenerationOptions {
    let mut sampling = OrderedJsonObject::new();
    if let Some(values) = value.get("samplingParams").and_then(Value::as_object) {
        for (name, value) in values {
            sampling.insert(name, OrderedJsonValue::from(value.clone()));
        }
    }
    SimpleGenerationOptions {
        max_output_tokens: value
            .get("maxTokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_str)
            .map(|value| match value {
                "minimal" => ReasoningLevel::Minimal,
                "low" => ReasoningLevel::Low,
                "medium" => ReasoningLevel::Medium,
                "high" => ReasoningLevel::High,
                "xhigh" => ReasoningLevel::Xhigh,
                "max" => ReasoningLevel::Max,
                other => panic!("unknown reasoning {other}"),
            }),
        sampling,
        ..Default::default()
    }
}

fn parse_context(value: &Value, model: &ModelDescriptor) -> Context {
    Context {
        schema_version: 1,
        system_prompt: value
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        messages: value["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, value)| parse_message(value, index, model))
            .collect(),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(parse_tool)
            .collect(),
    }
}

fn parse_message(value: &Value, index: usize, model: &ModelDescriptor) -> Message {
    let timestamp =
        Timestamp::from_unix_millis(value["timestamp"].as_i64().unwrap_or(FIXTURE_TIMESTAMP));
    match value["role"].as_str().expect("message role") {
        "user" => Message::User(agentprism_ai::UserMessage {
            id: MessageId::new(format!("fixture-user-{index}")),
            content: parse_user_content(&value["content"], index),
            timestamp,
        }),
        "assistant" => Message::Assistant(parse_assistant(value, index, model)),
        "toolResult" => Message::ToolResult(ToolResultMessage {
            id: MessageId::new(format!("fixture-result-{index}")),
            tool_call_id: ToolCallId::new(value["toolCallId"].as_str().unwrap_or_default()),
            tool_name: value["toolName"].as_str().unwrap_or_default().to_owned(),
            content: parse_tool_result_content(&value["content"], index),
            details: None,
            usage: None,
            added_tool_names: value["addedToolNames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            is_error: value["isError"].as_bool().unwrap_or(false),
            timestamp,
        }),
        other => panic!("unknown role {other}"),
    }
}

fn parse_user_content(value: &Value, message_index: usize) -> Vec<ContentBlock> {
    if let Some(text) = value.as_str() {
        return vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("fixture-user-{message_index}-0")),
            text: text.to_owned(),
        }];
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, block)| match block["type"].as_str() {
            Some("text") => Some(ContentBlock::Text {
                id: ContentBlockId::new(format!("fixture-user-{message_index}-{index}")),
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("image") => Some(ContentBlock::Image {
                id: ContentBlockId::new(format!("fixture-user-{message_index}-{index}")),
                data: block["data"].as_str().unwrap_or_default().to_owned(),
                mime_type: block["mimeType"].as_str().unwrap_or_default().to_owned(),
            }),
            _ => None,
        })
        .collect()
}

fn parse_tool_result_content(value: &Value, message_index: usize) -> Vec<ToolResultContent> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, block)| match block["type"].as_str() {
            Some("text") => Some(ToolResultContent::Text {
                id: ContentBlockId::new(format!("fixture-result-{message_index}-{index}")),
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("image") => Some(ToolResultContent::Image {
                id: ContentBlockId::new(format!("fixture-result-{message_index}-{index}")),
                data: block["data"].as_str().unwrap_or_default().to_owned(),
                mime_type: block["mimeType"].as_str().unwrap_or_default().to_owned(),
            }),
            _ => None,
        })
        .collect()
}

fn parse_assistant(value: &Value, index: usize, model: &ModelDescriptor) -> AssistantMessage {
    let provider = value["provider"]
        .as_str()
        .unwrap_or(model.common.model_ref.provider.as_str());
    let default_api = model.api.api_id();
    let api = value["api"].as_str().unwrap_or(default_api.as_str());
    let model_id = value["model"]
        .as_str()
        .unwrap_or(model.common.model_ref.model.as_str());
    let mut replay = ReplayEnvelope::new(ReplayScope::new(provider, api, model_id, model_id));
    let mut content = Vec::new();
    for (block_index, block) in value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = ContentBlockId::new(format!("fixture-assistant-{index}-{block_index}"));
        let signature = block
            .get("textSignature")
            .or_else(|| block.get("thinkingSignature"))
            .or_else(|| block.get("thoughtSignature"))
            .and_then(Value::as_str);
        if let Some(signature) = signature {
            let target = if block["type"].as_str() == Some("toolCall") {
                ReplayTarget::ToolCall(ToolCallId::new(block["id"].as_str().unwrap_or_default()))
            } else {
                ReplayTarget::ContentBlock(id.clone())
            };
            replay.items.push(ReplayItem {
                id: ReplayItemId::new(format!("fixture-replay-{index}-{block_index}")),
                ordinal: u32::try_from(block_index).expect("ordinal"),
                target,
                kind: ReplayKind::new(GOOGLE_THOUGHT_SIGNATURE_KIND),
                applicability: ReplayApplicability::ExactProviderApiModel,
                completeness: ReplayCompleteness::Complete,
                payload: OpaquePayload::Utf8(signature.to_owned()),
            });
        }
        match block["type"].as_str() {
            Some("text") => content.push(ContentBlock::Text {
                id,
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("thinking") => content.push(ContentBlock::Thinking {
                id,
                text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                redacted: false,
                replay_item: None,
            }),
            Some("toolCall") => content.push(ContentBlock::ToolCall {
                id,
                call: ToolCall {
                    id: ToolCallId::new(block["id"].as_str().unwrap_or_default()),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: block["arguments"].clone(),
                },
            }),
            _ => {}
        }
    }
    let raw = value["stopReason"].as_str().unwrap_or("stop");
    let reason = match raw {
        "stop" => AssistantFinishReason::Stop,
        "length" => AssistantFinishReason::Length,
        "toolUse" => AssistantFinishReason::ToolUse,
        "error" => AssistantFinishReason::Error,
        "aborted" => AssistantFinishReason::Aborted,
        other => panic!("unknown finish {other}"),
    };
    AssistantMessage {
        id: MessageId::new(format!("fixture-assistant-{index}")),
        provider: ProviderId::new(provider),
        api: agentprism_ai::ApiId::new(api),
        requested_model: ModelId::new(model_id),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content,
        replay,
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error: (reason == AssistantFinishReason::Error).then(|| agentprism_ai::PublicError {
                code: "fixture_error".to_owned(),
                message: value["errorMessage"]
                    .as_str()
                    .unwrap_or("fixture error")
                    .to_owned(),
                retryable: false,
                provider_code: None,
                status: None,
                request_id: None,
            }),
        },
        timestamp: Timestamp::from_unix_millis(
            value["timestamp"].as_i64().unwrap_or(FIXTURE_TIMESTAMP),
        ),
    }
}

fn parse_tool(value: &Value) -> ToolSpec {
    ToolSpec {
        schema_version: 1,
        name: value["name"].as_str().expect("tool name").to_owned(),
        description: value["description"].as_str().unwrap_or_default().to_owned(),
        parameters: value["parameters"].clone(),
        constrained_sampling: value
            .get("constrainedSampling")
            .and_then(Value::as_object)
            .map(|sampling| {
                ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema {
                    strict: match sampling["strict"].as_str().unwrap_or("prefer") {
                        "require" => JsonSchemaStrictMode::Require,
                        _ => JsonSchemaStrictMode::Prefer,
                    },
                })
            }),
    }
}

fn encode_fixture<A>(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    entrypoint: &str,
) -> Vec<u8>
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
    A::FullOptions: Default,
{
    let config = match (&model.api, A::API_ID) {
        (ApiModelConfig::GoogleGenerativeAi(config), "google-generative-ai")
        | (ApiModelConfig::GoogleVertex(config), "google-vertex") => config,
        _ => panic!("wrong fixture API"),
    };
    let typed = TypedModelDescriptor::<A> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let projected =
        transform_context_for_model(context, model, &Default::default(), &GoogleHandoff)
            .expect("Google projection")
            .context;
    let compat = GoogleCompat::default();
    let options = if entrypoint == "streamSimple" {
        let estimate = estimate_context_tokens(context).expect("estimate").tokens;
        A::lower_simple(
            SimpleLoweringContext {
                model: &typed,
                compat: &compat,
                effective_base_url: &model.common.base_url,
                estimated_input_tokens: estimate,
                available_context_tokens: model
                    .common
                    .limits
                    .context_window
                    .saturating_sub(estimate)
                    .saturating_sub(agentprism_ai::CONTEXT_SAFETY_TOKENS),
            },
            simple,
            &GoogleSimplePatch::default(),
        )
        .expect("simple lowering")
    } else {
        A::FullOptions::default()
    };
    let wire = A::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("Google encoding");
    OrderedJsonWriter::to_vec(&wire.into()).expect("wire JSON")
}

fn turn_two<A>(
    family: &str,
    case: &str,
    fixture: &Value,
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
) -> Vec<u8>
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
    A::FullOptions: Default,
{
    let events = decode_google_sse(
        &read_bytes(family, case, "response-turn-1.sse"),
        GoogleDecodeContext {
            message_id: MessageId::new(format!("fixture-response-{case}")),
            provider: model.common.model_ref.provider.clone(),
            api: agentprism_ai::ApiId::new(A::API_ID),
            requested_model: model.common.model_ref.model.clone(),
            pricing: model.common.pricing.clone(),
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        },
    );
    let assistant = events
        .iter()
        .rev()
        .find_map(|event| event.terminal_message())
        .expect("terminal Google response")
        .clone();
    // Architecture v2 part 2 §10.8 defines turn two only after the assembled
    // assistant has crossed the durable JSON boundary. Keeping this inside the
    // shared helper ensures both family wire suites and the two named replay
    // goldens exercise the required persistence round-trip.
    let persisted = serde_json::to_vec(&assistant).expect("persist assembled Google assistant");
    let assistant: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore assembled Google assistant");
    let mut turn_two = context.clone();
    turn_two.messages.push(Message::Assistant(assistant));
    for (offset, message) in fixture["turnTwoAppend"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        turn_two.messages.push(parse_message(
            message,
            context.messages.len() + 1 + offset,
            model,
        ));
    }
    encode_fixture::<A>(
        model,
        &turn_two,
        simple,
        fixture["entrypoint"].as_str().expect("entrypoint"),
    )
}

fn assert_family_fixtures<A>(family: &str)
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
    A::FullOptions: Default,
{
    for case in case_names(family) {
        let (fixture, model, context, simple) = parse_fixture(family, &case);
        let actual = encode_fixture::<A>(
            &model,
            &context,
            &simple,
            fixture["entrypoint"].as_str().expect("entrypoint"),
        );
        assert_eq!(
            actual,
            read_bytes(family, &case, "request-turn-1.body.json"),
            "turn one differs for {family}/{case}"
        );
        let actual = turn_two::<A>(family, &case, &fixture, &model, &context, &simple);
        assert_eq!(
            actual,
            read_bytes(family, &case, "request-turn-2.body.json"),
            "turn two differs for {family}/{case}"
        );
    }
}

/// Architecture v2 part 2 §10.8 `wire_google_generative_ai_pi_exact`;
/// pinned Pi basis: `api/google-generative-ai.ts` and `api/google-shared.ts`.
#[test]
fn wire_google_generative_ai_pi_exact() {
    assert_family_fixtures::<GoogleGenerativeAi>("google-generative-ai");
}

/// Architecture v2 part 2 §10.8 `wire_google_vertex_pi_exact`; pinned Pi
/// basis: `api/google-vertex.ts` and `api/google-shared.ts`.
#[test]
fn wire_google_vertex_pi_exact() {
    assert_family_fixtures::<GoogleVertex>("google-vertex");
}

/// Architecture v2 part 1 §3.4 and part 2 §3.3; pinned Pi basis:
/// `google-generative-ai.ts#buildParams` and `google-vertex.ts#buildParams`.
#[test]
fn google_full_thinking_enabled_without_selector_pi_exact() {
    let (_, generative_model, context, _) = parse_fixture("google-generative-ai", "text-only");
    let ApiModelConfig::GoogleGenerativeAi(config) = &generative_model.api else {
        unreachable!("Google fixture API")
    };
    let generative = TypedModelDescriptor::<GoogleGenerativeAi> {
        common: generative_model.common.clone(),
        config: config.clone(),
        extensions: generative_model.extensions.clone(),
    };
    let generative_options = GoogleOptions {
        thinking: Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: None,
        }),
        ..GoogleOptions::default()
    };
    let generative_wire = GoogleGenerativeAi::encode(
        EncodeContext {
            model: &generative,
            context: &context,
            compat: &GoogleCompat::default(),
            effective_base_url: &generative.common.base_url,
        },
        &generative_options,
    )
    .expect("enabled-only Generative options");

    let (_, vertex_model, context, _) = parse_fixture("google-vertex", "text-only");
    let ApiModelConfig::GoogleVertex(config) = &vertex_model.api else {
        unreachable!("Vertex fixture API")
    };
    let vertex = TypedModelDescriptor::<GoogleVertex> {
        common: vertex_model.common.clone(),
        config: config.clone(),
        extensions: vertex_model.extensions.clone(),
    };
    let vertex_options = GoogleVertexOptions {
        thinking: Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: None,
        }),
        ..GoogleVertexOptions::default()
    };
    let vertex_wire = GoogleVertex::encode(
        EncodeContext {
            model: &vertex,
            context: &context,
            compat: &GoogleCompat::default(),
            effective_base_url: &vertex.common.base_url,
        },
        &vertex_options,
    )
    .expect("enabled-only Vertex options");

    for wire in [generative_wire, vertex_wire] {
        let json = ordered_value_to_json(&wire.into());
        assert_eq!(
            json["generationConfig"]["thinkingConfig"],
            serde_json::json!({ "includeThoughts": true })
        );
    }

    let both = GoogleOptions {
        thinking: Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: Some(1234),
            level: Some(GoogleThinkingLevel::Low),
        }),
        ..GoogleOptions::default()
    };
    let wire = GoogleGenerativeAi::encode(
        EncodeContext {
            model: &generative,
            context: &context,
            compat: &GoogleCompat::default(),
            effective_base_url: &generative.common.base_url,
        },
        &both,
    )
    .expect("level takes precedence over budget");
    let json = ordered_value_to_json(&wire.into());
    assert_eq!(
        json["generationConfig"]["thinkingConfig"],
        serde_json::json!({ "includeThoughts": true, "thinkingLevel": "LOW" })
    );
}

fn terminal_from_sse(sse: &str, provider: &str, model: &str) -> AssistantMessage {
    terminal_from_sse_for_api(sse, provider, GoogleGenerativeAi::API_ID, model)
}

fn terminal_from_sse_for_api(
    sse: &str,
    provider: &str,
    api: &str,
    model: &str,
) -> AssistantMessage {
    decode_google_sse(
        sse.as_bytes(),
        GoogleDecodeContext {
            message_id: MessageId::new("google-test-message"),
            provider: ProviderId::new(provider),
            api: agentprism_ai::ApiId::new(api),
            requested_model: ModelId::new(model),
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: Default::default(),
            },
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        },
    )
    .into_iter()
    .find_map(|event| event.terminal_message().cloned())
    .expect("terminal message")
}

fn chunk(parts: &str) -> String {
    format!(
        "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{parts}]}},\"finishReason\":\"STOP\"}}]}}\n\n"
    )
}

fn assert_signature_target(message: &AssistantMessage, index: usize) {
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(message.content[index].id().clone())
    );
}

/// Architecture v2 part 2 §10.2
/// `google_thought_flag_not_signature_defines_thinking`; pinned Pi basis:
/// `google-thinking-signature.test.ts`.
#[test]
fn google_thought_flag_not_signature_defines_thinking() {
    let message = terminal_from_sse(
        &chunk("{\"text\":\"visible\",\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}"),
        "google",
        "gemini-3-test",
    );
    assert!(matches!(message.content[0], ContentBlock::Text { .. }));
}

/// Architecture v2 part 2 §10.2
/// `google_text_part_signature_stays_on_text_part`; pinned Pi basis:
/// `google-thinking-signature.test.ts`.
#[test]
fn google_text_part_signature_stays_on_text_part() {
    let message = terminal_from_sse(
        &chunk("{\"text\":\"visible\",\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}"),
        "google",
        "gemini-3-test",
    );
    assert_signature_target(&message, 0);
}

/// Architecture v2 part 2 §10.2
/// `google_thinking_part_signature_stays_on_thinking_part`; pinned Pi basis:
/// `google-thinking-signature.test.ts`.
#[test]
fn google_thinking_part_signature_stays_on_thinking_part() {
    let message = terminal_from_sse(
        &chunk(
            "{\"text\":\"reason\",\"thought\":true,\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}",
        ),
        "google",
        "gemini-3-test",
    );
    assert!(matches!(message.content[0], ContentBlock::Thinking { .. }));
    assert_signature_target(&message, 0);
}

/// Architecture v2 part 2 §10.2
/// `google_tool_call_signature_stays_on_function_call`; pinned Pi basis:
/// `google-thinking-signature.test.ts` and Google stream adapters.
#[test]
fn google_tool_call_signature_stays_on_function_call() {
    let events = decode_google_sse(
        chunk(
            "{\"functionCall\":{\"id\":\"call_1\",\"name\":\"read\",\"args\":{}},\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}",
        )
        .as_bytes(),
        GoogleDecodeContext {
            message_id: MessageId::new("google-tool-signature"),
            provider: ProviderId::new("google"),
            api: agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID),
            requested_model: ModelId::new("gemini-3-test"),
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: Default::default(),
            },
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        },
    );
    let replay_position = events
        .iter()
        .position(|event| {
            matches!(
                event,
                agentprism_ai::AssistantEvent::ReplayItemStarted { .. }
            )
        })
        .expect("replay start");
    let metadata_position = events
        .iter()
        .position(|event| {
            matches!(
                event,
                agentprism_ai::AssistantEvent::ToolCallMetadata { .. }
            )
        })
        .expect("tool metadata");
    assert!(replay_position < metadata_position);
    let message = events
        .iter()
        .find_map(agentprism_ai::AssistantEvent::terminal_message)
        .expect("terminal message");
    let ContentBlock::ToolCall { call, .. } = &message.content[0] else {
        panic!("expected tool call");
    };
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ToolCall(call.id.clone())
    );
    assert_eq!(
        message.replay.items[0].kind.as_str(),
        "google.genai.thought-signature"
    );
}

/// Architecture v2 part 2 §1.8 shared Google decoding; pinned Pi basis:
/// `google-generative-ai.ts:199` and `google-vertex.ts:216`, where
/// `functionCall.args ?? {}` normalizes explicit JSON null as well as omission.
#[test]
fn google_function_call_null_args_defaults_to_empty_object_pi_exact() {
    let response = chunk("{\"functionCall\":{\"id\":\"call_1\",\"name\":\"read\",\"args\":null}}");
    for (provider, api) in [
        ("google", GoogleGenerativeAi::API_ID),
        ("google-vertex", GoogleVertex::API_ID),
    ] {
        let message = terminal_from_sse_for_api(&response, provider, api, "gemini-3-test");
        let ContentBlock::ToolCall { call, .. } = &message.content[0] else {
            panic!("expected tool call for {api}");
        };
        assert_eq!(call.arguments, serde_json::json!({}), "API family {api}");
    }
}

/// Architecture v2 part 2 §10.2
/// `google_empty_signed_text_part_is_retained`; pinned Pi basis:
/// `google-shared-signed-empty-blocks.test.ts`.
#[test]
fn google_empty_signed_text_part_is_retained() {
    let message = terminal_from_sse(
        &chunk("{\"text\":\"\",\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}"),
        "google",
        "gemini-3-test",
    );
    assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text.is_empty()));
}

/// Architecture v2 part 2 §10.2
/// `google_empty_signed_thinking_part_is_retained`; pinned Pi basis:
/// `google-shared-signed-empty-blocks.test.ts`.
#[test]
fn google_empty_signed_thinking_part_is_retained() {
    let message = terminal_from_sse(
        &chunk(
            "{\"text\":\"\",\"thought\":true,\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}",
        ),
        "google",
        "gemini-3-test",
    );
    assert!(matches!(&message.content[0], ContentBlock::Thinking { text, .. } if text.is_empty()));
}

/// Architecture v2 part 2 §10.2
/// `google_stream_omission_does_not_clear_prior_signature`; pinned Pi basis:
/// `google-thinking-signature.test.ts`.
#[test]
fn google_stream_omission_does_not_clear_prior_signature() {
    let sse = format!(
        "{}{}",
        chunk_without_finish("{\"text\":\"a\",\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}"),
        chunk("{\"text\":\"b\"}")
    );
    let message = terminal_from_sse(&sse, "google", "gemini-3-test");
    assert_eq!(
        message.replay.items[0].as_utf8(),
        Some("AAAAAAAAAAAAAAAAAAAAAA==")
    );
}

fn chunk_without_finish(parts: &str) -> String {
    format!("data: {{\"candidates\":[{{\"content\":{{\"parts\":[{parts}]}}}}]}}\n\n")
}

fn one_text_with_replay(
    provider: &str,
    model: &str,
    signature: &str,
) -> (ModelDescriptor, Context) {
    let (_, target, _, _) = parse_fixture("google-generative-ai", "text-only");
    let mut context = Context::new(None);
    let block_id = ContentBlockId::new("signed-text");
    let mut replay = ReplayEnvelope::new(ReplayScope::new(
        provider,
        GoogleGenerativeAi::API_ID,
        model,
        model,
    ));
    replay.items.push(ReplayItem {
        id: ReplayItemId::new("signed-text-replay"),
        ordinal: 0,
        target: ReplayTarget::ContentBlock(block_id.clone()),
        kind: ReplayKind::new(GOOGLE_THOUGHT_SIGNATURE_KIND),
        applicability: ReplayApplicability::ExactProviderApiModel,
        completeness: ReplayCompleteness::Complete,
        payload: OpaquePayload::Utf8(signature.to_owned()),
    });
    context.messages.push(Message::Assistant(AssistantMessage {
        id: MessageId::new("signed-assistant"),
        provider: ProviderId::new(provider),
        api: agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID),
        requested_model: ModelId::new(model),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Text {
            id: block_id,
            text: String::new(),
        }],
        replay,
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
    }));
    (target, context)
}

/// Architecture v2 part 2 §10.2
/// `google_invalid_base64_signature_is_dropped`; pinned Pi basis:
/// `google-shared.ts#resolveThoughtSignature`.
#[test]
fn google_invalid_base64_signature_is_dropped() {
    for invalid in ["not base64", "AA=A", "====", "AAA"] {
        let (model, context) = one_text_with_replay("fixture-google", "gemini-3-fixture", invalid);
        let body = encode_fixture::<GoogleGenerativeAi>(
            &model,
            &context,
            &SimpleGenerationOptions::default(),
            "stream",
        );
        assert!(
            !String::from_utf8(body)
                .expect("UTF-8")
                .contains("thoughtSignature"),
            "accepted invalid signature {invalid:?}"
        );
    }
}

/// Architecture v2 part 2 §10.2
/// `google_signature_requires_same_provider_and_model`; pinned Pi basis:
/// `google-shared-signed-empty-blocks.test.ts`.
#[test]
fn google_signature_requires_same_provider_and_model() {
    for (source_provider, source_model) in [
        ("other-google", "gemini-3-fixture"),
        ("fixture-google", "other-model"),
    ] {
        let (model, context) =
            one_text_with_replay(source_provider, source_model, "AAAAAAAAAAAAAAAAAAAAAA==");
        let body = encode_fixture::<GoogleGenerativeAi>(
            &model,
            &context,
            &SimpleGenerationOptions::default(),
            "stream",
        );
        assert!(
            !String::from_utf8(body)
                .expect("UTF-8")
                .contains("thoughtSignature"),
            "signature crossed source scope {source_provider}/{source_model}"
        );
    }
}

/// Architecture v2 part 2 §10.2
/// `google_signature_never_moves_between_parts`; pinned Pi basis:
/// `google-thinking-signature.test.ts` protocol invariant.
#[test]
fn google_signature_never_moves_between_parts() {
    let message = terminal_from_sse(
        &chunk(
            "{\"text\":\"reason\",\"thought\":true,\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"},{\"functionCall\":{\"id\":\"call_1\",\"name\":\"read\",\"args\":{}}}",
        ),
        "google",
        "gemini-3-test",
    );
    assert_signature_target(&message, 0);
    assert_eq!(message.replay.items.len(), 1);
}

/// Architecture v2 part 2 §1.8 replay invariant R7; pinned Pi basis:
/// `google-generative-ai.ts` and `google-vertex.ts` provider-part iteration.
#[test]
fn google_replay_ordinals_include_unsigned_provider_parts_pi_exact() {
    let message = terminal_from_sse(
        &chunk(
            "{\"text\":\"plain\"},{\"functionCall\":{\"id\":\"call_1\",\"name\":\"read\",\"args\":{}}},{\"text\":\"signed\",\"thoughtSignature\":\"AAAAAAAAAAAAAAAAAAAAAA==\"}",
        ),
        "google",
        "gemini-3-test",
    );
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(message.replay.items[0].ordinal, 2);
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(message.content[2].id().clone())
    );
}

/// Architecture v2 part 2 §10.8
/// `google_tool_thought_signature_turn_two_pi_exact`; pinned Pi basis:
/// `google-generative-ai.ts` stream assembly plus `google-shared.ts` replay.
#[test]
fn google_tool_thought_signature_turn_two_pi_exact() {
    let family = "google-generative-ai";
    let case = "signed-thinking-replay";
    let (fixture, model, context, simple) = parse_fixture(family, case);
    assert_eq!(
        turn_two::<GoogleGenerativeAi>(family, case, &fixture, &model, &context, &simple),
        read_bytes(family, case, "request-turn-2.body.json")
    );
}

/// Architecture v2 part 2 §10.8
/// `google_empty_signed_part_turn_two_pi_exact`; pinned Pi basis:
/// `google-shared-signed-empty-blocks.test.ts`.
#[test]
fn google_empty_signed_part_turn_two_pi_exact() {
    let family = "google-generative-ai";
    let case = "redacted-encrypted-reasoning-replay";
    let (fixture, model, context, simple) = parse_fixture(family, case);
    assert_eq!(
        turn_two::<GoogleGenerativeAi>(family, case, &fixture, &model, &context, &simple),
        read_bytes(family, case, "request-turn-2.body.json")
    );
}

/// Architecture v2 part 2 §5.3 and §9.2; pinned Pi basis:
/// `google-vertex-api-key-resolution.test.ts` and `providers/google-vertex.ts`.
#[test]
fn google_vertex_api_key_resolution_send_and_local() {
    use agentprism_ai::{
        CancellationToken, LocalAuthResolver, LocalModels, LocalResolveAuthRequest, MapAuthContext,
        ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        GCP_VERTEX_CREDENTIALS_MARKER, google_vertex_auth_resolver,
        local_google_vertex_auth_resolver,
    };
    use futures_util::StreamExt;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let send_auth_transport = Arc::new(AdcTransport::default());
    let send_resolver = google_vertex_auth_resolver(send_auth_transport.clone());

    let key_context = MapAuthContext::new(
        BTreeMap::from([(
            "GOOGLE_CLOUD_API_KEY".to_owned(),
            "AIzaSyExampleRealisticLookingApiKey123456".to_owned(),
        )]),
        Vec::<String>::new(),
    );
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(key_context.clone());
    let resolved =
        futures_executor::block_on(send_resolver.resolve(request, CancellationToken::new()))
            .expect("Send key resolution")
            .expect("real key auth");
    assert_eq!(
        resolved
            .api_key
            .as_ref()
            .expect("resolved Vertex API key")
            .expose_secret(),
        "AIzaSyExampleRealisticLookingApiKey123456"
    );
    assert_eq!(
        resolved.headers["x-goog-api-key"],
        "AIzaSyExampleRealisticLookingApiKey123456"
    );
    assert_eq!(
        resolved.base_url.expect("Vertex Express endpoint").as_str(),
        "https://aiplatform.googleapis.com/v1"
    );

    let adc = BTreeMap::from([
        (
            "GOOGLE_CLOUD_API_KEY".to_owned(),
            "<authenticated>".to_owned(),
        ),
        ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
        ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
    ]);
    let adc_document = r#"{
        "type":"authorized_user",
        "client_id":"fixture-client",
        "client_secret":"fixture-secret",
        "refresh_token":"fixture-refresh",
        "token_uri":"https://oauth2.googleapis.com/token",
        "quota_project_id":"fixture-quota-project"
    }"#;
    let adc_context = MapAuthContext::new(adc, Vec::<String>::new()).with_file(
        agentprism_google_vertex::VERTEX_ADC_PATH,
        SecretString::new(adc_document),
    );
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(adc_context.clone());
    let resolved =
        futures_executor::block_on(send_resolver.resolve(request, CancellationToken::new()))
            .expect("Send ADC resolution")
            .expect("placeholder falls back to ADC");
    assert!(resolved.api_key.is_none());
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "fixture-quota-project"
    );
    assert_eq!(
        resolved.base_url.expect("ADC endpoint").as_str(),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1"
    );

    let marker_context = MapAuthContext::new(
        BTreeMap::from([
            (
                "GOOGLE_CLOUD_API_KEY".to_owned(),
                GCP_VERTEX_CREDENTIALS_MARKER.to_owned(),
            ),
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "marker-project".to_owned(),
            ),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "eu".to_owned()),
        ]),
        Vec::<String>::new(),
    )
    .with_file(
        agentprism_google_vertex::VERTEX_ADC_PATH,
        SecretString::new(adc_document),
    );
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(marker_context);
    let resolved =
        futures_executor::block_on(send_resolver.resolve(request, CancellationToken::new()))
            .expect("marker resolution")
            .expect("marker selects ADC");
    assert_eq!(
        resolved.base_url.expect("multi-region endpoint").as_str(),
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/marker-project/locations/eu"
    );

    let (_, mut custom_model, _, _) = parse_fixture("google-vertex", "text-only");
    custom_model.common.base_url =
        Url::parse("https://proxy.example.com/v1/projects/custom/locations/global")
            .expect("custom Vertex base URL");
    let mut request =
        ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), Some(custom_model));
    request.auth_context = Arc::new(key_context.clone());
    let resolved =
        futures_executor::block_on(send_resolver.resolve(request, CancellationToken::new()))
            .expect("custom base resolution")
            .expect("real key with custom base");
    assert_eq!(
        resolved.base_url.expect("custom endpoint").as_str(),
        "https://proxy.example.com/v1/projects/custom/locations/global"
    );

    let (_, mut custom_model, _, _) = parse_fixture("google-vertex", "text-only");
    custom_model.common.base_url =
        Url::parse("https://proxy.example.com/v1/projects/custom/locations/global")
            .expect("custom Vertex ADC base URL");
    let mut request =
        ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), Some(custom_model));
    request.auth_context = Arc::new(adc_context.clone());
    let resolved =
        futures_executor::block_on(send_resolver.resolve(request, CancellationToken::new()))
            .expect("custom ADC base resolution")
            .expect("ADC with custom base");
    assert_eq!(
        resolved.base_url.expect("custom ADC endpoint").as_str(),
        "https://proxy.example.com/v1/projects/custom/locations/global"
    );
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "fixture-quota-project"
    );

    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(key_context);
    let local_auth_transport = Rc::new(AdcTransport::default());
    let resolver = local_google_vertex_auth_resolver(local_auth_transport.clone());
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Local key resolution")
    .expect("local real key auth");
    assert_eq!(
        resolved
            .api_key
            .as_ref()
            .expect("resolved local Vertex API key")
            .expose_secret(),
        "AIzaSyExampleRealisticLookingApiKey123456"
    );
    assert_eq!(
        resolved.headers["x-goog-api-key"],
        "AIzaSyExampleRealisticLookingApiKey123456"
    );
    assert_eq!(
        resolved
            .base_url
            .expect("local Vertex Express endpoint")
            .as_str(),
        "https://aiplatform.googleapis.com/v1"
    );

    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(adc_context.clone());
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Local ADC resolution")
    .expect("local placeholder falls back to ADC");
    assert!(resolved.api_key.is_none());
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        resolved.base_url.expect("local ADC endpoint").as_str(),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1"
    );
    assert_eq!(send_auth_transport.requests().len(), 3);
    assert_eq!(local_auth_transport.requests().len(), 1);

    // Exercise the Models pipeline, not merely the resolver: ADC must be
    // exchanged and its bearer token must authenticate the Vertex request.
    let pipeline_transport = Arc::new(AdcTransport::default());
    let model = google_vertex_models().expect("Vertex models")[0]
        .common
        .model_ref
        .clone();
    let models = Models::builder()
        .auth_context(Arc::new(adc_context.clone()))
        .provider(
            google_vertex_provider(pipeline_transport.clone())
                .expect("Vertex pipeline registration"),
        )
        .build()
        .expect("Vertex Models");
    let mut options = SimpleGenerationOptions::default();
    options
        .headers
        .insert("User-Agent".to_owned(), Some("custom-agent".to_owned()));
    let stream = futures_executor::block_on(models.stream_simple(
        ModelRequest {
            model,
            context: Context::new(None),
            options,
        },
        CancellationToken::new(),
    ))
    .expect("Vertex ADC stream");
    let _events = futures_executor::block_on(stream.collect::<Vec<_>>());
    let requests = pipeline_transport.requests();
    assert_eq!(requests.len(), 2, "ADC exchange then Vertex request");
    assert!(
        !requests[1].url.as_str().contains("{location}"),
        "catalog endpoint placeholder must be resolved"
    );
    assert_eq!(
        requests[1].headers[http::header::USER_AGENT],
        "custom-agent"
    );
    assert_eq!(requests[1].headers[http::header::ACCEPT], "*/*");
    assert_eq!(
        requests[1].headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        requests[1].headers["x-goog-user-project"],
        "fixture-quota-project"
    );
    assert_eq!(
        requests[1].auth_headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        requests[1].auth_headers["x-goog-user-project"],
        "fixture-quota-project"
    );

    let local_pipeline_transport = Rc::new(AdcTransport::default());
    let model = google_vertex_models().expect("local Vertex models")[0]
        .common
        .model_ref
        .clone();
    let models = LocalModels::builder()
        .auth_context(Rc::new(adc_context))
        .provider(
            local_google_vertex_provider(local_pipeline_transport.clone())
                .expect("local Vertex pipeline registration"),
        )
        .build()
        .expect("local Vertex Models");
    let stream = futures_executor::block_on(models.stream_simple(
        ModelRequest {
            model,
            context: Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .expect("local Vertex ADC stream");
    let _events = futures_executor::block_on(stream.collect::<Vec<_>>());
    let requests = local_pipeline_transport.requests();
    assert_eq!(requests.len(), 2, "local ADC exchange then Vertex request");
    assert_eq!(
        requests[1].headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        requests[1].headers["x-goog-user-project"],
        "fixture-quota-project"
    );
    assert_eq!(
        requests[1].auth_headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        requests[1].auth_headers["x-goog-user-project"],
        "fixture-quota-project"
    );
}

/// Architecture v2 part 2 §5.3, §6, §9.2, and §10.7; pinned Pi basis:
/// `providers/google-vertex.ts:55-94` checks only API-key or ADC
/// file/project/location configuration.
#[test]
fn google_vertex_check_auth_is_configuration_only_send_and_local() {
    use agentprism_ai::{
        AuthCheck, AuthSource, CancellationToken, CredentialType, LocalModels, MapAuthContext,
        Models,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let context = MapAuthContext::new(
        BTreeMap::from([
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "configured-project".to_owned(),
            ),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
        ]),
        [agentprism_google_vertex::VERTEX_ADC_PATH.to_owned()],
    );
    let models = Models::builder()
        .auth_context(Arc::new(context.clone()))
        .provider(
            google_vertex_provider(Arc::new(NeverTransport)).expect("Send Vertex registration"),
        )
        .build()
        .expect("Send Vertex Models");
    let check = futures_executor::block_on(
        models.check_auth(ProviderId::new("google-vertex"), CancellationToken::new()),
    )
    .expect("Send Vertex auth check")
    .expect("Send ADC configuration");
    assert_eq!(
        check,
        AuthCheck {
            source: Some(AuthSource::new("gcloud application default credentials")),
            credential_type: CredentialType::ApiKey,
        }
    );

    let models = LocalModels::builder()
        .auth_context(Rc::new(context))
        .provider(
            local_google_vertex_provider(Rc::new(NeverTransport))
                .expect("Local Vertex registration"),
        )
        .build()
        .expect("Local Vertex Models");
    let check = futures_executor::block_on(
        models.check_auth(ProviderId::new("google-vertex"), CancellationToken::new()),
    )
    .expect("Local Vertex auth check")
    .expect("Local ADC configuration");
    assert_eq!(
        check,
        AuthCheck {
            source: Some(AuthSource::new("gcloud application default credentials")),
            credential_type: CredentialType::ApiKey,
        }
    );
}

/// Architecture v2 part 2 §6 and §9.2; pinned Pi basis:
/// `providers/google-vertex.ts:74-88`.
#[test]
fn google_vertex_adc_missing_scope_skips_token_exchange_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let adc_document = r#"{
        "type":"authorized_user",
        "client_id":"fixture-client",
        "client_secret":"fixture-secret",
        "refresh_token":"fixture-refresh",
        "token_uri":"https://oauth2.googleapis.com/token"
    }"#;
    for (case, environment) in [
        (
            "missing project",
            BTreeMap::from([("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned())]),
        ),
        (
            "missing location",
            BTreeMap::from([("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned())]),
        ),
    ] {
        let context = MapAuthContext::new(environment, Vec::<String>::new()).with_file(
            agentprism_google_vertex::VERTEX_ADC_PATH,
            SecretString::new(adc_document),
        );

        let send_transport = Arc::new(AdcTransport::default());
        let resolver = google_vertex_auth_resolver(send_transport.clone());
        let mut request =
            ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
        request.auth_context = Arc::new(context.clone());
        let resolved = futures_executor::block_on(AuthResolver::resolve(
            resolver.as_ref(),
            request,
            CancellationToken::new(),
        ))
        .unwrap_or_else(|error| panic!("Send {case} resolution failed: {error}"));
        assert!(resolved.is_none(), "Send {case} must not resolve ADC");
        assert!(
            send_transport.requests().is_empty(),
            "Send {case} must not exchange an ADC token"
        );

        let local_transport = Rc::new(AdcTransport::default());
        let resolver = local_google_vertex_auth_resolver(local_transport.clone());
        let mut request =
            LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
        request.auth_context = Rc::new(context);
        let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
            resolver.as_ref(),
            request,
            CancellationToken::new(),
        ))
        .unwrap_or_else(|error| panic!("Local {case} resolution failed: {error}"));
        assert!(resolved.is_none(), "Local {case} must not resolve ADC");
        assert!(
            local_transport.requests().is_empty(),
            "Local {case} must not exchange an ADC token"
        );
    }
}

/// Architecture v2 part 2 §6 and §10.7; pinned Pi basis:
/// `google-vertex.ts:353-367` and GoogleAuth 10.6.2 `fromJSON`. GoogleAuth
/// never treats an arbitrary credential-file `access_token` as validated ADC.
#[test]
fn google_vertex_adc_invalid_type_with_access_token_is_rejected_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let context = MapAuthContext::new(
        BTreeMap::from([
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
        ]),
        Vec::<String>::new(),
    )
    .with_file(
        agentprism_google_vertex::VERTEX_ADC_PATH,
        SecretString::new(r#"{"type":"not_google_adc","access_token":"must-not-be-trusted"}"#),
    );

    let send_transport = Arc::new(AdcTransport::default());
    let resolver = google_vertex_auth_resolver(send_transport.clone());
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    let error = futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect_err("invalid Send ADC type must be rejected");
    assert_eq!(error.code(), "invalid_vertex_adc");
    assert!(send_transport.requests().is_empty());

    let local_transport = Rc::new(AdcTransport::default());
    let resolver = local_google_vertex_auth_resolver(local_transport.clone());
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    let error = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect_err("invalid local ADC type must be rejected");
    assert_eq!(error.code(), "invalid_vertex_adc");
    assert!(local_transport.requests().is_empty());
}

/// Architecture v2 part 2 §6 and §10.7; pinned Pi basis:
/// `google-vertex.ts:353-367` and GoogleAuth 10.6.2 `UserRefreshClient`, whose
/// OAuth endpoint is fixed independently of a credential-file `token_uri`.
#[test]
fn google_vertex_authorized_user_ignores_token_uri_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let context = MapAuthContext::new(
        BTreeMap::from([
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
        ]),
        Vec::<String>::new(),
    )
    .with_file(
        agentprism_google_vertex::VERTEX_ADC_PATH,
        SecretString::new(
            r#"{
                "type":"authorized_user",
                "client_id":"fixture-client",
                "client_secret":"fixture-secret",
                "refresh_token":"fixture-refresh",
                "token_uri":"https://attacker.invalid/token"
            }"#,
        ),
    );

    let send_transport = Arc::new(AdcTransport::default());
    let resolver = google_vertex_auth_resolver(send_transport.clone());
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Send authorized-user resolution")
    .expect("Send authorized-user auth");
    assert_eq!(
        send_transport.requests()[0].url.as_str(),
        "https://oauth2.googleapis.com/token"
    );

    let local_transport = Rc::new(AdcTransport::default());
    let resolver = local_google_vertex_auth_resolver(local_transport.clone());
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("local authorized-user resolution")
    .expect("local authorized-user auth");
    assert_eq!(
        local_transport.requests()[0].url.as_str(),
        "https://oauth2.googleapis.com/token"
    );
}

/// Architecture v2 part 2 §1.8, §9.2, and §10.7; pinned Pi basis:
/// `google-vertex.ts:353-367` delegates ADC headers to `@google/genai`, whose
/// locked GoogleAuth 10.6.2 `UserRefreshClient` carries `quota_project_id` as
/// `x-goog-user-project`.
#[test]
fn google_vertex_authorized_user_quota_project_header_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let context = MapAuthContext::new(
        BTreeMap::from([
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
        ]),
        Vec::<String>::new(),
    )
    .with_file(
        agentprism_google_vertex::VERTEX_ADC_PATH,
        SecretString::new(
            r#"{
                "type":"authorized_user",
                "client_id":"fixture-client",
                "client_secret":"fixture-secret",
                "refresh_token":"fixture-refresh",
                "quota_project_id":"authorized-user-quota-project"
            }"#,
        ),
    );

    let resolver = google_vertex_auth_resolver(Arc::new(AdcTransport::default()));
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    let resolved = futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Send authorized-user quota resolution")
    .expect("Send authorized-user auth");
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "authorized-user-quota-project"
    );

    let resolver = local_google_vertex_auth_resolver(Rc::new(AdcTransport::default()));
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Local authorized-user quota resolution")
    .expect("Local authorized-user auth");
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "authorized-user-quota-project"
    );
}

/// Architecture v2 part 2 §6 and §9.2; pinned Pi basis:
/// `google-vertex.ts:353-367` passing `keyFilename` to GoogleAuth, whose
/// supported ADC families include external and impersonated credentials.
#[test]
fn google_vertex_non_user_adc_delegates_to_host_adapter_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        VertexAdcCredentialType, google_vertex_auth_resolver_with_adc_adapter,
        local_google_vertex_auth_resolver_with_adc_adapter,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let credential_path = "/fixtures/external-account.json";
    let external_account = r#"{
        "type":"external_account",
        "audience":"//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider",
        "subject_token_type":"urn:ietf:params:oauth:token-type:jwt",
        "token_url":"https://sts.googleapis.com/v1/token",
        "quota_project_id":"delegated-credential-quota-project",
        "credential_source":{"file":"/fixtures/subject-token"}
    }"#;
    let context = MapAuthContext::new(
        BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
                credential_path.to_owned(),
            ),
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
            (
                "GOOGLE_CLOUD_QUOTA_PROJECT".to_owned(),
                "delegated-override-quota-project".to_owned(),
            ),
        ]),
        Vec::<String>::new(),
    )
    .with_file(credential_path, SecretString::new(external_account));

    let send_transport = Arc::new(AdcTransport::default());
    let send_adapter = Arc::new(RecordingVertexAdcAdapter::default());
    let resolver =
        google_vertex_auth_resolver_with_adc_adapter(send_transport.clone(), send_adapter.clone());
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    let resolved = futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Send delegated ADC resolution")
    .expect("Send delegated ADC auth");
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-delegated-adc-token"
    );
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "delegated-override-quota-project"
    );
    assert!(send_transport.requests().is_empty());
    let requests = send_adapter.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].credential_path, credential_path);
    assert_eq!(
        requests[0].credential_type,
        VertexAdcCredentialType::ExternalAccount
    );
    assert_eq!(
        requests[0].scopes,
        [agentprism_google_vertex::VERTEX_CLOUD_PLATFORM_SCOPE]
    );
    assert_eq!(
        requests[0].credential_json.expose_secret(),
        external_account
    );
    assert_eq!(
        requests[0].quota_project_id.as_deref(),
        Some("delegated-override-quota-project")
    );

    let local_transport = Rc::new(AdcTransport::default());
    let local_adapter = Rc::new(RecordingVertexAdcAdapter::default());
    let resolver = local_google_vertex_auth_resolver_with_adc_adapter(
        local_transport.clone(),
        local_adapter.clone(),
    );
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("local delegated ADC resolution")
    .expect("local delegated ADC auth");
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-delegated-adc-token"
    );
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "delegated-override-quota-project"
    );
    assert!(local_transport.requests().is_empty());
    let requests = local_adapter.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].credential_type,
        VertexAdcCredentialType::ExternalAccount
    );
    assert_eq!(
        requests[0].quota_project_id.as_deref(),
        Some("delegated-override-quota-project")
    );
}

/// Architecture v2 part 2 §6 and §9.2; pinned Pi basis:
/// `providers/google-vertex.ts:21-23,51-60` and
/// `api/google-vertex.ts:353-367`.
#[test]
fn google_vertex_service_account_resolution_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use base64::Engine as _;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let credential_path = "/fixtures/vertex-service-account.json";
    let document = serde_json::to_string(&serde_json::json!({
        "type": "service_account",
        "project_id": "service-file-project",
        "private_key_id": "fixture-key-id",
        "private_key": SERVICE_ACCOUNT_PRIVATE_KEY,
        "client_email": "vertex-fixture@service-file-project.iam.gserviceaccount.com",
        "client_id": "123456789",
        "token_uri": "https://oauth2.googleapis.com/token"
    }))
    .expect("service-account fixture JSON");
    let context = MapAuthContext::new(
        BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
                credential_path.to_owned(),
            ),
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "per-login-project".to_owned(),
            ),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
        ]),
        Vec::<String>::new(),
    )
    .with_file(credential_path, SecretString::new(document));

    let send_transport = Arc::new(AdcTransport::default());
    let resolver = google_vertex_auth_resolver(send_transport.clone());
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    let resolved = futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Send service-account resolution")
    .expect("Send service-account auth");
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(
        resolved
            .base_url
            .expect("service-account endpoint")
            .as_str(),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/per-login-project/locations/us-central1"
    );
    let requests = send_transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.as_str(),
        "https://oauth2.googleapis.com/token"
    );
    let form = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:jwt-bearer"
    );
    let segments = form["assertion"].split('.').collect::<Vec<_>>();
    assert_eq!(segments.len(), 3);
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segments[1])
            .expect("service-account JWT claims"),
    )
    .expect("service-account JWT JSON");
    assert_eq!(
        claims["iss"],
        "vertex-fixture@service-file-project.iam.gserviceaccount.com"
    );
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
    assert_eq!(
        claims["exp"].as_u64(),
        claims["iat"].as_u64().map(|iat| iat + 3_600)
    );

    let local_transport = Rc::new(AdcTransport::default());
    let resolver = local_google_vertex_auth_resolver(local_transport.clone());
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Local service-account resolution")
    .expect("Local service-account auth");
    assert_eq!(
        resolved.headers[http::header::AUTHORIZATION],
        "Bearer fixture-adc-token"
    );
    assert_eq!(local_transport.requests().len(), 1);
}

/// Architecture v2 part 2 §1.8, §9.2, and §10.7; pinned Pi basis:
/// `google-vertex.ts:353-367` delegates ADC headers to `@google/genai`, whose
/// locked GoogleAuth 10.6.2 overrides credential `quota_project_id` with
/// `GOOGLE_CLOUD_QUOTA_PROJECT` before producing `x-goog-user-project`.
#[test]
fn google_vertex_service_account_quota_project_override_send_and_local() {
    use agentprism_ai::{
        AuthResolver, CancellationToken, LocalAuthResolver, LocalResolveAuthRequest,
        MapAuthContext, ProviderDescriptor, ResolveAuthRequest, SecretString,
    };
    use agentprism_google_vertex::{
        google_vertex_auth_resolver, local_google_vertex_auth_resolver,
    };
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;

    let credential_path = "/fixtures/vertex-service-account-quota.json";
    let document = serde_json::to_string(&serde_json::json!({
        "type": "service_account",
        "project_id": "service-file-project",
        "private_key_id": "fixture-key-id",
        "private_key": SERVICE_ACCOUNT_PRIVATE_KEY,
        "client_email": "vertex-fixture@service-file-project.iam.gserviceaccount.com",
        "client_id": "123456789",
        "quota_project_id": "service-account-metadata-quota-project"
    }))
    .expect("service-account quota fixture JSON");
    let context = MapAuthContext::new(
        BTreeMap::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
                credential_path.to_owned(),
            ),
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "test-project".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
            (
                "GOOGLE_CLOUD_QUOTA_PROJECT".to_owned(),
                "environment-override-quota-project".to_owned(),
            ),
        ]),
        Vec::<String>::new(),
    )
    .with_file(credential_path, SecretString::new(document));

    let resolver = google_vertex_auth_resolver(Arc::new(AdcTransport::default()));
    let mut request = ResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Arc::new(context.clone());
    let resolved = futures_executor::block_on(AuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Send service-account quota resolution")
    .expect("Send service-account auth");
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "environment-override-quota-project"
    );

    let resolver = local_google_vertex_auth_resolver(Rc::new(AdcTransport::default()));
    let mut request =
        LocalResolveAuthRequest::isolated(ProviderDescriptor::new("google-vertex"), None);
    request.auth_context = Rc::new(context);
    let resolved = futures_executor::block_on(LocalAuthResolver::resolve(
        resolver.as_ref(),
        request,
        CancellationToken::new(),
    ))
    .expect("Local service-account quota resolution")
    .expect("Local service-account auth");
    assert_eq!(
        resolved.headers["x-goog-user-project"],
        "environment-override-quota-project"
    );
}

fn ordered_value_to_json(value: &OrderedJsonValue) -> Value {
    serde_json::from_slice(&OrderedJsonWriter::to_vec(value).expect("ordered JSON encoding"))
        .expect("ordered JSON parses")
}

fn google_simple_body<A>(
    family: &str,
    model_id: &str,
    levels: ThinkingLevelMap<String>,
    reasoning: Option<ReasoningLevel>,
    budgets: Option<ThinkingBudgets>,
    reasoning_capable: bool,
) -> Value
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
    A::FullOptions: Default,
{
    let (_, mut model, context, mut simple) = parse_fixture(family, "text-only");
    model.common.model_ref.model = ModelId::new(model_id);
    model.common.reasoning = reasoning_capable;
    match &mut model.api {
        ApiModelConfig::GoogleGenerativeAi(config) | ApiModelConfig::GoogleVertex(config) => {
            config.thinking_levels = levels;
        }
        _ => unreachable!("Google fixture API"),
    }
    simple.reasoning = reasoning;
    simple.thinking_budgets = budgets;
    serde_json::from_slice(&encode_fixture::<A>(
        &model,
        &context,
        &simple,
        "streamSimple",
    ))
    .expect("Google body JSON")
}

fn lower_google_options<A>(
    family: &str,
    model_id: &str,
    levels: ThinkingLevelMap<String>,
    simple: &SimpleGenerationOptions,
) -> Result<GoogleOptions, agentprism_ai::LoweringError>
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            FullOptions = GoogleOptions,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
{
    let (_, mut model, _, _) = parse_fixture(family, "text-only");
    model.common.model_ref.model = ModelId::new(model_id);
    let config = match &mut model.api {
        ApiModelConfig::GoogleGenerativeAi(config) | ApiModelConfig::GoogleVertex(config) => {
            config.thinking_levels = levels;
            config.clone()
        }
        _ => unreachable!("Google fixture API"),
    };
    let typed = TypedModelDescriptor::<A> {
        common: model.common,
        config,
        extensions: model.extensions,
    };
    A::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &GoogleCompat::default(),
            effective_base_url: &typed.common.base_url,
            estimated_input_tokens: 1,
            available_context_tokens: 100_000,
        },
        simple,
        &GoogleSimplePatch::default(),
    )
}

fn thinking_config(body: &Value) -> Option<&serde_json::Map<String, Value>> {
    body.get("generationConfig")?
        .get("thinkingConfig")?
        .as_object()
}

/// Architecture v2 part 2 §10.2 stop/error terminal semantics; pinned Pi
/// basis: `google-raw-stop-reason.test.ts`.
#[test]
fn google_raw_stop_reason_and_tool_precedence_pi_exact() {
    for (api, provider, raw) in [
        (
            GoogleGenerativeAi::API_ID,
            "google",
            "MALFORMED_FUNCTION_CALL",
        ),
        (GoogleVertex::API_ID, "google-vertex", "SAFETY"),
    ] {
        let message = terminal_from_sse_for_api(
            &format!(
                "data: {{\"candidates\":[{{\"finishReason\":\"{raw}\"}}],\"usageMetadata\":{{\"promptTokenCount\":1,\"candidatesTokenCount\":0,\"totalTokenCount\":1}}}}\n\n"
            ),
            provider,
            api,
            "gemini-test",
        );
        assert_eq!(message.finish.reason, AssistantFinishReason::Error);
        assert_eq!(message.finish.raw_provider_reason.as_deref(), Some(raw));
        assert_eq!(
            message
                .finish
                .error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some(format!("Provider stopped with: {raw}").as_str())
        );
    }

    for (raw, expected) in [
        ("MAX_TOKENS", AssistantFinishReason::Length),
        ("STOP", AssistantFinishReason::ToolUse),
    ] {
        let message = terminal_from_sse_for_api(
            &format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"functionCall\":{{\"id\":\"call-1\",\"name\":\"echo\",\"args\":{{\"value\":\"truncated\"}}}}}}]}},\"finishReason\":\"{raw}\"}}]}}\n\n"
            ),
            "google",
            GoogleGenerativeAi::API_ID,
            "gemini-3-test",
        );
        assert_eq!(message.finish.reason, expected);
        assert_eq!(message.finish.raw_provider_reason.as_deref(), Some(raw));
        assert!(matches!(message.content[0], ContentBlock::ToolCall { .. }));
    }
}

/// Architecture v2 part 2 §3.5 and §10.8 tool conversion; pinned Pi basis:
/// `google-shared-convert-tools.test.ts`.
#[test]
fn google_convert_tools_schema_modes_pi_exact() {
    let original = serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "urn:test",
        "$comment": "metadata",
        "$defs": {"ignored": {"type": "number"}},
        "definitions": {"ignored": {"type": "integer"}},
        "type": "object",
        "properties": {
            "deep": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$id": "urn:nested",
                "$ref": "#/$defs/someDef",
                "type": "string"
            }
        },
        "required": ["deep"]
    });
    let tool = ToolSpec {
        schema_version: 1,
        name: "test_tool".to_owned(),
        description: "A test tool".to_owned(),
        parameters: original.clone(),
        constrained_sampling: None,
    };
    let open_api = convert_google_tools(std::slice::from_ref(&tool), true, true)
        .expect("OpenAPI conversion")
        .expect("tool declaration");
    let open_api = ordered_value_to_json(&open_api);
    let parameters = &open_api[0]["functionDeclarations"][0]["parameters"];
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["properties"]["deep"]["$ref"], "#/$defs/someDef");
    for meta in ["$schema", "$id", "$comment", "$defs", "definitions"] {
        assert!(parameters.get(meta).is_none());
    }
    assert!(parameters["properties"]["deep"].get("$schema").is_none());
    assert!(parameters["properties"]["deep"].get("$id").is_none());
    assert_eq!(
        tool.parameters, original,
        "conversion must not mutate input"
    );

    let json_schema = convert_google_tools(std::slice::from_ref(&tool), false, true)
        .expect("JSON Schema conversion")
        .expect("tool declaration");
    let json_schema = ordered_value_to_json(&json_schema);
    assert_eq!(
        json_schema[0]["functionDeclarations"][0]["parametersJsonSchema"]["$schema"],
        "http://json-schema.org/draft-07/schema#"
    );
    assert!(
        convert_google_tools(&[], false, true)
            .expect("empty conversion")
            .is_none()
    );

    let mut strict = tool;
    strict.constrained_sampling = Some(ConstrainedSampling::Config(
        ConstrainedSamplingConfig::JsonSchema {
            strict: JsonSchemaStrictMode::Require,
        },
    ));
    assert!(supports_google_strict_tool_sampling(
        "gemini-3.1-pro-preview"
    ));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));
    assert!(convert_google_tools(&[strict], false, false).is_err());
}

/// Architecture v2 part 2 §10.8 replay request semantics; pinned Pi basis:
/// `google-shared-gemini3-unsigned-tool-call.test.ts`.
#[test]
fn google_unsigned_tool_calls_never_gain_synthetic_signature_pi_exact() {
    for family in ["google-generative-ai", "google-vertex"] {
        let (fixture, model, context, simple) = parse_fixture(family, "multiple-tool-calls");
        let bytes = if family == GoogleGenerativeAi::API_ID {
            turn_two::<GoogleGenerativeAi>(
                family,
                "multiple-tool-calls",
                &fixture,
                &model,
                &context,
                &simple,
            )
        } else {
            turn_two::<GoogleVertex>(
                family,
                "multiple-tool-calls",
                &fixture,
                &model,
                &context,
                &simple,
            )
        };
        let body: Value = serde_json::from_slice(&bytes).expect("unsigned tool body");
        let serialized = serde_json::to_string(&body).expect("body serialization");
        assert!(!serialized.contains("skip_thought_signature_validator"));
        assert!(!serialized.contains("thoughtSignature"));
        let calls = body["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .flat_map(|content| content["parts"].as_array().into_iter().flatten())
            .filter_map(|part| part.get("functionCall"))
            .collect::<Vec<_>>();
        let responses = body["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .flat_map(|content| content["parts"].as_array().into_iter().flatten())
            .filter_map(|part| part.get("functionResponse"))
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .iter()
                .filter_map(|call| call["id"].as_str())
                .collect::<Vec<_>>(),
            ["call_fixture_0001", "call_fixture_0002"]
        );
        assert_eq!(
            responses
                .iter()
                .filter_map(|response| response["id"].as_str())
                .collect::<Vec<_>>(),
            ["call_fixture_0001", "call_fixture_0002"]
        );
    }

    for (expected, model) in [
        (false, "gemini-2.5-flash"),
        (true, "gemini-3.6-flash"),
        (true, "claude-sonnet-4-5"),
        (true, "gpt-oss-120b"),
    ] {
        assert_eq!(requires_google_tool_call_id(model), expected, "{model}");
    }
}

/// Architecture v2 part 2 §10.8 multimodal tool results; pinned Pi basis:
/// `google-shared-image-tool-result-routing.test.ts` and
/// `image-tool-result.test.ts`.
#[test]
fn google_image_tool_result_routing_by_model_generation_pi_exact() {
    let (_, mut model, context, simple) =
        parse_fixture("google-generative-ai", "tool-result-images");
    model.common.model_ref.model = ModelId::new("gemini-3.1-pro-preview");
    let gemini_three: Value = serde_json::from_slice(&encode_fixture::<GoogleGenerativeAi>(
        &model, &context, &simple, "stream",
    ))
    .expect("Gemini 3 body");
    let three_contents = gemini_three["contents"].as_array().expect("contents");
    let three_response = three_contents
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .find_map(|part| part.get("functionResponse"))
        .expect("Gemini 3 function response");
    assert_eq!(three_response["parts"].as_array().map(Vec::len), Some(1));
    assert!(
        !three_contents
            .iter()
            .any(|content| { content["parts"][0]["text"].as_str() == Some("Tool result image:") })
    );

    model.common.model_ref.model = ModelId::new("gemini-2.5-flash");
    let gemini_two: Value = serde_json::from_slice(&encode_fixture::<GoogleGenerativeAi>(
        &model, &context, &simple, "stream",
    ))
    .expect("Gemini 2 body");
    let two_contents = gemini_two["contents"].as_array().expect("contents");
    let two_response = two_contents
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .find_map(|part| part.get("functionResponse"))
        .expect("Gemini 2 function response");
    assert!(two_response.get("parts").is_none());
    assert!(two_contents.iter().any(|content| {
        content["parts"][0]["text"].as_str() == Some("Tool result image:")
            && content["parts"][1].get("inlineData").is_some()
    }));
}

#[derive(Clone, Copy)]
struct FixedRetryJitter;

impl RetryJitter for FixedRetryJitter {
    fn sample(&self, range: &std::ops::RangeInclusive<f64>) -> f64 {
        *range.end()
    }
}

/// Architecture v2 part 2 §2.4; pinned Pi basis:
/// `google-shared-retry.test.ts` and `utils/provider-retry.ts`.
#[test]
fn google_retry_status_classification_pi_exact() {
    let classifier = DefaultRetryClassifier::new(FixedRetryJitter);
    let mut policy = RetryPolicy::default();
    assert_eq!(policy.max_retries, 0, "unset maxRetries means no retry");
    policy.max_retries = 1;
    assert!(matches!(
        classifier.classify(
            &AttemptFailure::http(0, 429, http::HeaderMap::new(), "rate limited"),
            &policy,
        ),
        RetryDecision::RetryAfter(delay) if delay == Duration::from_millis(500)
    ));
    assert_eq!(
        classifier.classify(
            &AttemptFailure::http(0, 400, http::HeaderMap::new(), "bad request"),
            &policy,
        ),
        RetryDecision::DoNotRetry
    );
}

/// Architecture v2 part 2 §3.3 and §5.1; pinned Pi basis:
/// `google-thinking-disable.test.ts` and both Google simple adapters.
#[test]
fn google_thinking_disable_wire_modes_pi_exact() {
    let defaults = ThinkingLevelMap::default();
    let gemini_two = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-2.5-flash",
        defaults.clone(),
        None,
        None,
        true,
    );
    assert_eq!(thinking_config(&gemini_two).unwrap()["thinkingBudget"], 0);

    let flash = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.1-flash-lite",
        defaults.clone(),
        None,
        None,
        true,
    );
    assert_eq!(thinking_config(&flash).unwrap()["thinkingLevel"], "MINIMAL");
    assert!(
        thinking_config(&flash)
            .unwrap()
            .get("includeThoughts")
            .is_none()
    );

    let pro = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.1-pro-preview",
        defaults.clone(),
        None,
        None,
        true,
    );
    assert_eq!(thinking_config(&pro).unwrap()["thinkingLevel"], "LOW");

    let gemma = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemma-4-26b-a4b-it",
        defaults.clone(),
        None,
        None,
        true,
    );
    assert_eq!(thinking_config(&gemma).unwrap()["thinkingLevel"], "MINIMAL");
    let vertex_gemma = google_simple_body::<GoogleVertex>(
        "google-vertex",
        "gemma-4-26b-a4b-it",
        defaults.clone(),
        None,
        None,
        true,
    );
    assert_eq!(thinking_config(&vertex_gemma).unwrap()["thinkingBudget"], 0);

    let non_reasoning = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-2.5-flash",
        defaults,
        Some(ReasoningLevel::High),
        None,
        false,
    );
    assert!(thinking_config(&non_reasoning).is_none());
}

/// Architecture v2 part 2 §3.3 and §5.1; pinned Pi basis:
/// `google-thinking-level-map.test.ts`.
#[test]
fn google_thinking_level_map_pi_exact() {
    let uppercase = ThinkingLevelMap {
        high: Some(LevelSupport::Value("LOW".to_owned())),
        xhigh: Some(LevelSupport::Value("high".to_owned())),
        max: Some(LevelSupport::Value("HIGH".to_owned())),
        ..Default::default()
    };
    let high = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.7-flash",
        uppercase.clone(),
        Some(ReasoningLevel::High),
        None,
        true,
    );
    assert_eq!(thinking_config(&high).unwrap()["thinkingLevel"], "LOW");
    let xhigh = google_simple_body::<GoogleVertex>(
        "google-vertex",
        "gemini-3.7-flash",
        uppercase,
        Some(ReasoningLevel::Xhigh),
        None,
        true,
    );
    assert_eq!(thinking_config(&xhigh).unwrap()["thinkingLevel"], "HIGH");

    let budget_levels = ThinkingLevelMap {
        xhigh: Some(LevelSupport::Value("high".to_owned())),
        max: Some(LevelSupport::Value("high".to_owned())),
        ..Default::default()
    };
    let custom = ThinkingBudgets {
        high: Some(1_234),
        ..ThinkingBudgets::default()
    };
    let budget = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-2.5-flash",
        budget_levels.clone(),
        Some(ReasoningLevel::Xhigh),
        Some(custom),
        true,
    );
    assert_eq!(thinking_config(&budget).unwrap()["thinkingBudget"], 1_234);
    let vertex_budget = google_simple_body::<GoogleVertex>(
        "google-vertex",
        "gemini-2.5-flash",
        budget_levels,
        Some(ReasoningLevel::Max),
        Some(ThinkingBudgets {
            high: Some(4_321),
            ..ThinkingBudgets::default()
        }),
        true,
    );
    assert_eq!(
        thinking_config(&vertex_budget).unwrap()["thinkingBudget"],
        4_321
    );

    let generative_flash_lite = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-2.5-flash-lite",
        ThinkingLevelMap::default(),
        Some(ReasoningLevel::Minimal),
        None,
        true,
    );
    assert_eq!(
        thinking_config(&generative_flash_lite).unwrap()["thinkingBudget"],
        512
    );
    let vertex_flash_lite = google_simple_body::<GoogleVertex>(
        "google-vertex",
        "gemini-2.5-flash-lite",
        ThinkingLevelMap::default(),
        Some(ReasoningLevel::Minimal),
        None,
        true,
    );
    assert_eq!(
        thinking_config(&vertex_flash_lite).unwrap()["thinkingBudget"],
        128
    );

    // Pi distinguishes omission from an explicitly supplied budget object.
    // The shared default value 1024 must override Google's model default 512.
    let explicit_default_budgets = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-2.5-flash-lite",
        ThinkingLevelMap::default(),
        Some(ReasoningLevel::Minimal),
        Some(ThinkingBudgets::default()),
        true,
    );
    assert_eq!(
        thinking_config(&explicit_default_budgets).unwrap()["thinkingBudget"],
        1_024
    );

    // Extended levels omitted from the model map clamp to the ordinary high
    // level before the Google mapping is applied.
    let clamped = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.7-flash",
        ThinkingLevelMap::default(),
        Some(ReasoningLevel::Max),
        None,
        true,
    );
    assert_eq!(thinking_config(&clamped).unwrap()["thinkingLevel"], "HIGH");

    // Pi distinguishes omission (disable) from an explicit `off` level,
    // whose direct Google mapping is high.
    let explicit_off = google_simple_body::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.7-flash",
        ThinkingLevelMap::default(),
        Some(ReasoningLevel::Off),
        None,
        true,
    );
    assert_eq!(
        thinking_config(&explicit_off).unwrap()["thinkingLevel"],
        "HIGH"
    );
    assert_eq!(
        thinking_config(&explicit_off).unwrap()["includeThoughts"],
        true
    );

    let mut invalid_options = SimpleGenerationOptions {
        reasoning: Some(ReasoningLevel::Xhigh),
        ..Default::default()
    };
    let invalid = lower_google_options::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.7-flash",
        ThinkingLevelMap {
            xhigh: Some(LevelSupport::Value("extreme".to_owned())),
            ..Default::default()
        },
        &invalid_options,
    )
    .expect_err("unknown mapped value");
    assert!(invalid.to_string().contains(
        "unsupported Google thinking level mapping for fixture-google/gemini-3.7-flash: xhigh -> extreme"
    ));

    let mapped_off = lower_google_options::<GoogleGenerativeAi>(
        "google-generative-ai",
        "gemini-3.7-flash",
        ThinkingLevelMap {
            high: Some(LevelSupport::Value("off".to_owned())),
            ..Default::default()
        },
        &SimpleGenerationOptions {
            reasoning: Some(ReasoningLevel::High),
            ..Default::default()
        },
    )
    .expect_err("non-off level mapped to off is invalid");
    assert!(mapped_off.to_string().contains(
        "unsupported Google thinking level mapping for fixture-google/gemini-3.7-flash: high -> off"
    ));

    invalid_options.reasoning = Some(ReasoningLevel::Max);
    invalid_options.reasoning_fallback = agentprism_ai::ReasoningFallback::Strict;
    assert!(matches!(
        lower_google_options::<GoogleGenerativeAi>(
            "google-generative-ai",
            "gemini-3.7-flash",
            ThinkingLevelMap::default(),
            &invalid_options,
        ),
        Err(agentprism_ai::LoweringError::UnsupportedReasoningLevel {
            requested: ReasoningLevel::Max
        })
    ));
}

/// Architecture v2 part 1 §3.9 correction and part 2 §10.8 turn-two
/// planning; pinned Pi basis: `utils/estimate.ts#calculateContextTokens` and
/// both Google usage decoders.
#[test]
fn google_provider_total_tokens_drive_turn_two_context_planning_pi_exact() {
    let message = terminal_from_sse(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":0,\"cachedContentTokenCount\":0,\"totalTokenCount\":20}}\n\n",
        "google",
        "gemini-3-test",
    );
    assert_eq!(message.usage.input_tokens, 12);
    assert_eq!(message.usage.output_tokens, 5);
    assert_eq!(message.usage.total_tokens, Some(20));
    assert_eq!(agentprism_ai::calculate_context_tokens(&message.usage), 20);

    let family = "google-generative-ai";
    let case = "max-output-clamp";
    let (fixture, model, context, simple) = parse_fixture(family, case);
    assert_eq!(
        turn_two::<GoogleGenerativeAi>(family, case, &fixture, &model, &context, &simple),
        read_bytes(family, case, "request-turn-2.body.json")
    );
}

fn assert_google_zero_total_fallback<A>(family: &str)
where
    A: ApiFamily<
            Compat = GoogleCompat,
            ModelConfig = GoogleModelConfig,
            OptionsPatch = GoogleSimplePatch,
            WireRequest = OrderedJsonObject,
        >,
    A::FullOptions: Default,
{
    let (_, mut model, _, mut simple) = parse_fixture(family, "text-only");
    let message = terminal_from_sse_for_api(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3,\"cachedContentTokenCount\":2,\"totalTokenCount\":0}}\n\n",
        model.common.model_ref.provider.as_str(),
        A::API_ID,
        model.common.model_ref.model.as_str(),
    );
    assert_eq!(message.usage.input_tokens, 10);
    assert_eq!(message.usage.output_tokens, 8);
    assert_eq!(message.usage.cache_read_tokens, Some(2));
    assert_eq!(message.usage.total_tokens, Some(0));
    assert_eq!(agentprism_ai::calculate_context_tokens(&message.usage), 20);

    let persisted = serde_json::to_vec(&message).expect("persist zero-total Google response");
    let message: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore zero-total Google response");
    let context = Context {
        schema_version: 1,
        system_prompt: None,
        messages: vec![Message::Assistant(message)],
        tools: Vec::new(),
    };
    let estimate = estimate_context_tokens(&context).expect("zero-total context estimate");
    assert_eq!(estimate.usage_tokens, 20);
    assert_eq!(estimate.tokens, 20);

    model.common.limits.context_window = 4_126;
    simple.max_output_tokens = Some(100);
    let body: Value = serde_json::from_slice(&encode_fixture::<A>(
        &model,
        &context,
        &simple,
        "streamSimple",
    ))
    .expect("zero-total Google request body");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 10);
}

/// Architecture v2 part 1 §3.9 correction and part 2 §3.4; pinned Pi basis:
/// `utils/estimate.ts#calculateContextTokens` and both Google usage decoders.
#[test]
fn google_zero_total_tokens_fall_back_to_components_for_context_planning_pi_exact() {
    assert_google_zero_total_fallback::<GoogleGenerativeAi>("google-generative-ai");
    assert_google_zero_total_fallback::<GoogleVertex>("google-vertex");
}

/// Architecture v2 part 1 §3.9 and part 2 §5.2; pinned Pi basis:
/// both Google adapters' `calculateCost(model, output.usage)` calls.
#[test]
fn google_terminal_cost_is_calculated_pi_exact() {
    let pricing = ModelPricing {
        default: TokenPriceRates {
            input: MoneyRate::new(1_000_000),
            output: MoneyRate::new(2_000_000),
            cache_read: MoneyRate::new(500_000),
            cache_write: MoneyRate::new(0),
        },
        request_wide_tiers: Vec::new(),
        cache_write_retention: Default::default(),
    };
    let terminal = decode_google_sse(
        b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3,\"cachedContentTokenCount\":2,\"totalTokenCount\":20}}\n\n",
        GoogleDecodeContext {
            message_id: MessageId::new("google-priced-message"),
            provider: ProviderId::new("google"),
            api: agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID),
            requested_model: ModelId::new("gemini-priced"),
            pricing,
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        },
    )
    .into_iter()
    .find_map(|event| event.terminal_message().cloned())
    .expect("priced terminal message");
    let cost = terminal.cost.expect("Google terminal cost");
    assert_eq!(cost.currency.as_str(), "USD");
    assert_eq!(cost.micros, 27);
}

#[derive(Debug)]
struct NeverTransport;

impl agentprism_ai::HttpTransport for NeverTransport {
    fn execute(
        &self,
        _request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::SendBoxFuture<
        '_,
        Result<agentprism_ai::HttpResponse, agentprism_ai::TransportError>,
    > {
        Box::pin(async {
            Err(agentprism_ai::TransportError::new(
                "unexpected",
                "not executed",
            ))
        })
    }
}

impl agentprism_ai::LocalHttpTransport for NeverTransport {
    fn execute(
        &self,
        _request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::LocalBoxFuture<
        '_,
        Result<agentprism_ai::LocalHttpResponse, agentprism_ai::TransportError>,
    > {
        Box::pin(async {
            Err(agentprism_ai::TransportError::new(
                "unexpected",
                "not executed",
            ))
        })
    }
}

#[derive(Debug, Default)]
struct AdcTransport {
    requests: std::sync::Mutex<Vec<agentprism_ai::HttpRequest>>,
}

impl AdcTransport {
    fn requests(&self) -> Vec<agentprism_ai::HttpRequest> {
        self.requests.lock().expect("ADC request lock").clone()
    }
}

impl agentprism_ai::HttpTransport for AdcTransport {
    fn execute(
        &self,
        request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::SendBoxFuture<
        '_,
        Result<agentprism_ai::HttpResponse, agentprism_ai::TransportError>,
    > {
        let is_token_exchange = request.url.as_str() == "https://oauth2.googleapis.com/token";
        self.requests
            .lock()
            .expect("ADC request lock")
            .push(request);
        Box::pin(async move {
            Ok(agentprism_ai::HttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                if is_token_exchange {
                    br#"{"access_token":"fixture-adc-token","expires_in":3600,"token_type":"Bearer"}"#
                        .to_vec()
                } else {
                    b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                        .to_vec()
                },
            ))
        })
    }
}

impl agentprism_ai::LocalHttpTransport for AdcTransport {
    fn execute(
        &self,
        request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::LocalBoxFuture<
        '_,
        Result<agentprism_ai::LocalHttpResponse, agentprism_ai::TransportError>,
    > {
        let is_token_exchange = request.url.as_str() == "https://oauth2.googleapis.com/token";
        self.requests
            .lock()
            .expect("ADC request lock")
            .push(request);
        Box::pin(async move {
            Ok(agentprism_ai::LocalHttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                if is_token_exchange {
                    br#"{"access_token":"fixture-adc-token","expires_in":3600,"token_type":"Bearer"}"#
                        .to_vec()
                } else {
                    b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                        .to_vec()
                },
            ))
        })
    }
}

#[derive(Debug, Default)]
struct RecordingVertexAdcAdapter {
    requests: std::sync::Mutex<Vec<agentprism_google_vertex::VertexAdcTokenRequest>>,
}

impl RecordingVertexAdcAdapter {
    fn requests(&self) -> Vec<agentprism_google_vertex::VertexAdcTokenRequest> {
        self.requests
            .lock()
            .expect("Vertex ADC adapter request lock")
            .clone()
    }
}

impl agentprism_google_vertex::VertexAdcCredentialAdapter for RecordingVertexAdcAdapter {
    fn resolve_access_token(
        &self,
        request: agentprism_google_vertex::VertexAdcTokenRequest,
        cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::SendBoxFuture<
        '_,
        Result<agentprism_ai::SecretString, agentprism_ai::AuthError>,
    > {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| agentprism_ai::AuthError::Cancelled)?;
            self.requests
                .lock()
                .expect("Vertex ADC adapter request lock")
                .push(request);
            Ok(agentprism_ai::SecretString::new(
                "fixture-delegated-adc-token",
            ))
        })
    }
}

impl agentprism_google_vertex::LocalVertexAdcCredentialAdapter for RecordingVertexAdcAdapter {
    fn resolve_access_token(
        &self,
        request: agentprism_google_vertex::VertexAdcTokenRequest,
        cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::LocalBoxFuture<
        '_,
        Result<agentprism_ai::SecretString, agentprism_ai::AuthError>,
    > {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| agentprism_ai::AuthError::Cancelled)?;
            self.requests
                .lock()
                .expect("local Vertex ADC adapter request lock")
                .push(request);
            Ok(agentprism_ai::SecretString::new(
                "fixture-delegated-adc-token",
            ))
        })
    }
}

#[derive(Debug, Default)]
struct CapturingTransport {
    requests: std::sync::Mutex<Vec<agentprism_ai::HttpRequest>>,
}

impl agentprism_ai::HttpTransport for CapturingTransport {
    fn execute(
        &self,
        request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::SendBoxFuture<
        '_,
        Result<agentprism_ai::HttpResponse, agentprism_ai::TransportError>,
    > {
        self.requests.lock().expect("request lock").push(request);
        Box::pin(async {
            Ok(agentprism_ai::HttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                    .to_vec(),
            ))
        })
    }
}

impl agentprism_ai::LocalHttpTransport for CapturingTransport {
    fn execute(
        &self,
        request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::LocalBoxFuture<
        '_,
        Result<agentprism_ai::LocalHttpResponse, agentprism_ai::TransportError>,
    > {
        self.requests.lock().expect("request lock").push(request);
        Box::pin(async {
            Ok(agentprism_ai::LocalHttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                    .to_vec(),
            ))
        })
    }
}

#[derive(Debug)]
struct LocalNeverTransport;

impl agentprism_ai::LocalHttpTransport for LocalNeverTransport {
    fn execute(
        &self,
        _request: agentprism_ai::HttpRequest,
        _cancellation: agentprism_ai::CancellationToken,
    ) -> agentprism_ai::LocalBoxFuture<
        '_,
        Result<agentprism_ai::LocalHttpResponse, agentprism_ai::TransportError>,
    > {
        Box::pin(async {
            Err(agentprism_ai::TransportError::new(
                "unexpected",
                "not executed",
            ))
        })
    }
}

fn resolved_google_request(
    model: ModelDescriptor,
    context: Context,
    endpoint: &str,
    api: &str,
) -> agentprism_ai::ResolvedApiRequest {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-goog-api-key",
        http::HeaderValue::from_static("fixture-key"),
    );
    agentprism_ai::ResolvedApiRequest {
        model,
        context,
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: agentprism_ai::ApiRequestOptions::default(),
        endpoint: Url::parse(endpoint).expect("test endpoint"),
        headers,
        auth_headers: http::HeaderMap::new(),
        api_key: Some(SecretString::new("fixture-key")),
        api: agentprism_ai::ApiId::new(api),
        payload_transforms: std::sync::Arc::from([]),
        response_observers: std::sync::Arc::from([]),
        attempt_middleware: std::sync::Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: std::sync::Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_resolved_google_request(
    model: ModelDescriptor,
    context: Context,
    endpoint: &str,
    api: &str,
) -> agentprism_ai::LocalResolvedApiRequest {
    let send = resolved_google_request(model, context, endpoint, api);
    agentprism_ai::LocalResolvedApiRequest {
        model: send.model,
        context: send.context,
        options: send.options,
        full_options: send.full_options,
        request_options: send.request_options,
        endpoint: send.endpoint,
        headers: send.headers,
        auth_headers: send.auth_headers,
        api_key: send.api_key,
        api: send.api,
        payload_transforms: std::rc::Rc::from([]),
        response_observers: std::rc::Rc::from([]),
        attempt_middleware: std::rc::Rc::from([]),
        retry_policy: send.retry_policy,
        timeout: send.timeout,
        retry_classifier: std::rc::Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

/// Architecture v2 part 2 §10.8 Google wire conformance; pinned Pi basis:
/// `packages/ai/test/empty.test.ts` Google Generative AI matrix.
#[test]
fn google_empty_message_matrix_pi_exact() {
    let (_, model, _, _) = parse_fixture("google-generative-ai", "text-only");
    let cases = [
        serde_json::json!({
            "messages": [{"role": "user", "content": []}]
        }),
        serde_json::json!({
            "messages": [{"role": "user", "content": ""}]
        }),
        serde_json::json!({
            "messages": [{"role": "user", "content": "   "}]
        }),
        serde_json::json!({
            "messages": [
                {"role": "user", "content": "First message"},
                {
                    "role": "assistant",
                    "content": [],
                    "provider": "google",
                    "api": "google-generative-ai",
                    "model": "gemini-test",
                    "stopReason": "stop",
                    "usage": {
                        "input": 0,
                        "output": 0,
                        "cacheRead": 0,
                        "cacheWrite": 0
                    }
                },
                {"role": "user", "content": "Second message"}
            ]
        }),
    ];
    let capture = std::sync::Arc::new(CapturingTransport::default());
    let registration = google_provider(capture.clone()).expect("capturing Google provider");
    let api = &registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)];
    for case in cases {
        let context = parse_context(&case, &model);
        futures_executor::block_on(api.stream(
            resolved_google_request(
                model.clone(),
                context,
                "https://generativelanguage.googleapis.com/v1beta",
                GoogleGenerativeAi::API_ID,
            ),
            agentprism_ai::CancellationToken::new(),
        ))
        .expect("empty-message request is accepted");
    }
    assert_eq!(capture.requests.lock().expect("request lock").len(), 4);
}

/// Architecture v2 part 2 §9.2 and §10.7; pinned Pi basis:
/// `google-generative-ai.ts:83-85`, which requires `options.apiKey` before
/// constructing the SDK client regardless of custom request headers.
#[test]
fn google_generative_ai_authorization_only_is_authentication_send_and_local() {
    let (_, model, context, _) = parse_fixture("google-generative-ai", "text-only");

    let send_transport = std::sync::Arc::new(CapturingTransport::default());
    let registration = google_provider(send_transport.clone()).expect("Send Google provider");
    let mut request = resolved_google_request(
        model.clone(),
        context.clone(),
        "https://generativelanguage.googleapis.com/v1beta",
        GoogleGenerativeAi::API_ID,
    );
    request.api_key = None;
    request.headers.remove("x-goog-api-key");
    request.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer custom-only"),
    );
    let error = futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)]
            .stream(request, agentprism_ai::CancellationToken::new()),
    )
    .expect_err("Authorization-only Developer API request must fail");
    assert_eq!(error.kind, agentprism_ai::AiErrorKind::Authentication);
    assert!(
        send_transport
            .requests
            .lock()
            .expect("Send authorization-only request lock")
            .is_empty(),
        "Send transport must not be invoked"
    );

    let local_transport = std::rc::Rc::new(CapturingTransport::default());
    let registration =
        local_google_provider(local_transport.clone()).expect("local Google provider");
    let mut request = local_resolved_google_request(
        model,
        context,
        "https://generativelanguage.googleapis.com/v1beta",
        GoogleGenerativeAi::API_ID,
    );
    request.api_key = None;
    request.headers.remove("x-goog-api-key");
    request.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer custom-only"),
    );
    let error = futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)]
            .stream(request, agentprism_ai::CancellationToken::new()),
    )
    .expect_err("Authorization-only local Developer API request must fail");
    assert_eq!(error.kind, agentprism_ai::AiErrorKind::Authentication);
    assert!(
        local_transport
            .requests
            .lock()
            .expect("local authorization-only request lock")
            .is_empty(),
        "local transport must not be invoked"
    );
}

fn assert_google_erased_tool_choice_patch(family: &str, api: &str, endpoint: &str) {
    let (_, model, context, _) = parse_fixture(family, "one-tool-call");
    let capture = std::sync::Arc::new(CapturingTransport::default());
    let registration = match api {
        GoogleGenerativeAi::API_ID => {
            google_provider(capture.clone()).expect("capturing Google provider")
        }
        GoogleVertex::API_ID => {
            google_vertex_provider(capture.clone()).expect("capturing Vertex provider")
        }
        other => panic!("unknown Google API family {other}"),
    };
    let make_request = |choice: &str| {
        let mut request = resolved_google_request(model.clone(), context.clone(), endpoint, api);
        request.options.api_options = Some(ErasedApiOptionsPatch {
            api: agentprism_ai::ApiId::new(api),
            schema_version: 1,
            value: serde_json::value::RawValue::from_string(format!(
                "{{\"toolChoice\":\"{choice}\"}}"
            ))
            .expect("erased Google patch JSON"),
        });
        request
    };
    let implementation = &registration.apis[&agentprism_ai::ApiId::new(api)];
    futures_executor::block_on(
        implementation.stream(make_request("any"), agentprism_ai::CancellationToken::new()),
    )
    .expect("Pi-compatible lowercase erased patch");
    let requests = capture.requests.lock().expect("captured patch request");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("Google patch body JSON");
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    drop(requests);

    let error = futures_executor::block_on(
        implementation.stream(make_request("ANY"), agentprism_ai::CancellationToken::new()),
    )
    .expect_err("uppercase patch is outside pinned Pi's options contract");
    assert_eq!(error.kind, agentprism_ai::AiErrorKind::InvalidRequest);
    assert_eq!(
        capture
            .requests
            .lock()
            .expect("captured patch requests")
            .len(),
        1,
        "invalid erased patch must fail before transport"
    );

    assert_eq!(
        serde_json::to_value(GoogleSimplePatch {
            tool_choice: Some(GoogleToolChoice::Any),
        })
        .expect("canonical Google patch"),
        serde_json::json!({"toolChoice": "any"})
    );
}

/// Architecture v2 part 2 §3.3; pinned Pi basis:
/// `google-generative-ai.ts#GoogleOptions.toolChoice`.
#[test]
fn google_generative_ai_erased_tool_choice_patch_uses_pi_lowercase() {
    assert_google_erased_tool_choice_patch(
        "google-generative-ai",
        GoogleGenerativeAi::API_ID,
        "https://generativelanguage.googleapis.com/v1beta",
    );
}

/// Architecture v2 part 2 §3.3; pinned Pi basis:
/// `google-vertex.ts#GoogleVertexOptions.toolChoice`.
#[test]
fn google_vertex_erased_tool_choice_patch_uses_pi_lowercase() {
    assert_google_erased_tool_choice_patch(
        "google-vertex",
        GoogleVertex::API_ID,
        "https://aiplatform.googleapis.com/v1",
    );
}

/// Architecture v2 part 2 §1.8 and §5.1; pinned Pi basis:
/// `providers/google.ts`, `providers/google-vertex.ts`, and their pinned
/// generated catalog inputs.
#[derive(Clone, Copy)]
enum PinnedGoogleLevelProfile {
    Default,
    NoOff,
    Pro,
    Gemma,
}

#[derive(Clone, Copy)]
struct PinnedGoogleCatalogModel {
    id: &'static str,
    name: &'static str,
    input: i128,
    output: i128,
    cache_read: i128,
    context_window: u64,
    max_output_tokens: u32,
    levels: PinnedGoogleLevelProfile,
}

macro_rules! pinned_google_model {
    ($id:expr, $name:expr, $input:expr, $output:expr, $cache_read:expr, $context:expr, $max:expr, $levels:expr $(,)?) => {
        PinnedGoogleCatalogModel {
            id: $id,
            name: $name,
            input: $input,
            output: $output,
            cache_read: $cache_read,
            context_window: $context,
            max_output_tokens: $max,
            levels: $levels,
        }
    };
}

const PINNED_GOOGLE_CATALOG: &[PinnedGoogleCatalogModel] = &[
    pinned_google_model!(
        "deep-research-max-preview-04-2026",
        "Deep Research Max Preview (Apr-21-2026)",
        2_000_000,
        12_000_000,
        200_000,
        131_072,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "deep-research-preview-04-2026",
        "Deep Research Preview (Apr-21-2026)",
        2_000_000,
        12_000_000,
        200_000,
        131_072,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-computer-use-preview-10-2025",
        "Gemini 2.5 Computer Use Preview 10-2025",
        1_250_000,
        10_000_000,
        0,
        131_072,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-flash",
        "Gemini 2.5 Flash",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-flash-lite",
        "Gemini 2.5 Flash-Lite",
        100_000,
        400_000,
        10_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-pro",
        "Gemini 2.5 Pro",
        1_250_000,
        10_000_000,
        125_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        500_000,
        3_000_000,
        50_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-flash-lite",
        "Gemini 3.1 Flash Lite",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-flash-lite-image",
        "Nano Banana 2 Lite",
        250_000,
        30_000_000,
        0,
        65_536,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite Preview",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-flash-live-preview",
        "Gemini 3.1 Flash Live Preview",
        750_000,
        4_500_000,
        0,
        131_072,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Pro,
    ),
    pinned_google_model!(
        "gemini-3.1-pro-preview-customtools",
        "Gemini 3.1 Pro Preview Custom Tools",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Pro,
    ),
    pinned_google_model!(
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.5-flash-lite",
        "Gemini 3.5 Flash Lite",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.6-flash",
        "Gemini 3.6 Flash",
        1_500_000,
        7_500_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.7-flash",
        "Gemini 3.7 Flash",
        750_000,
        3_750_000,
        75_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-flash-latest",
        "Gemini Flash Latest",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-flash-lite-latest",
        "Gemini Flash-Lite Latest",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-robotics-er-1.6-preview",
        "Gemini Robotics-ER 1.6 Preview",
        1_000_000,
        5_000_000,
        0,
        131_072,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemma-4-26b-a4b-it",
        "Gemma 4 26B A4B IT",
        0,
        0,
        0,
        262_144,
        32_768,
        PinnedGoogleLevelProfile::Gemma,
    ),
    pinned_google_model!(
        "gemma-4-31b-it",
        "Gemma 4 31B IT",
        0,
        0,
        0,
        262_144,
        32_768,
        PinnedGoogleLevelProfile::Gemma,
    ),
];

const PINNED_GOOGLE_VERTEX_CATALOG: &[PinnedGoogleCatalogModel] = &[
    pinned_google_model!(
        "gemini-2.5-flash",
        "Gemini 2.5 Flash",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-flash-lite",
        "Gemini 2.5 Flash-Lite",
        100_000,
        400_000,
        10_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-2.5-pro",
        "Gemini 2.5 Pro",
        1_250_000,
        10_000_000,
        125_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Default,
    ),
    pinned_google_model!(
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        500_000,
        3_000_000,
        50_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-flash-lite",
        "Gemini 3.1 Flash Lite",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Pro,
    ),
    pinned_google_model!(
        "gemini-3.1-pro-preview-customtools",
        "Gemini 3.1 Pro Preview Custom Tools",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::Pro,
    ),
    pinned_google_model!(
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.5-flash-lite",
        "Gemini 3.5 Flash Lite",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.6-flash",
        "Gemini 3.6 Flash",
        1_500_000,
        7_500_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-3.7-flash",
        "Gemini 3.7 Flash",
        750_000,
        3_750_000,
        75_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-flash-latest",
        "Gemini Flash Latest",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
    pinned_google_model!(
        "gemini-flash-lite-latest",
        "Gemini Flash-Lite Latest",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        PinnedGoogleLevelProfile::NoOff,
    ),
];

fn pinned_google_thinking_levels(profile: PinnedGoogleLevelProfile) -> ThinkingLevelMap<String> {
    let unsupported = || Some(LevelSupport::Unsupported);
    let value = |value: &str| Some(LevelSupport::Value(value.to_owned()));
    match profile {
        PinnedGoogleLevelProfile::Default => ThinkingLevelMap::default(),
        PinnedGoogleLevelProfile::NoOff => ThinkingLevelMap {
            off: unsupported(),
            ..Default::default()
        },
        PinnedGoogleLevelProfile::Pro => ThinkingLevelMap {
            off: unsupported(),
            minimal: unsupported(),
            low: value("LOW"),
            medium: unsupported(),
            high: value("HIGH"),
            ..Default::default()
        },
        PinnedGoogleLevelProfile::Gemma => ThinkingLevelMap {
            off: unsupported(),
            minimal: value("MINIMAL"),
            low: unsupported(),
            medium: unsupported(),
            high: value("HIGH"),
            ..Default::default()
        },
    }
}

fn pinned_google_catalog_descriptors(
    snapshot: &[PinnedGoogleCatalogModel],
    provider: &str,
    api: &str,
    base_url: &str,
) -> Vec<ModelDescriptor> {
    snapshot
        .iter()
        .map(|model| {
            let config = GoogleModelConfig {
                thinking_levels: pinned_google_thinking_levels(model.levels),
            };
            ModelDescriptor {
                common: CommonModelDescriptor {
                    model_ref: ModelRef::new(provider, model.id),
                    display_name: model.name.to_owned(),
                    base_url: Url::parse(base_url).expect("pinned catalog URL"),
                    modalities: ModalityCapabilities {
                        input: [Modality::Text, Modality::Image].into_iter().collect(),
                        output: [Modality::Text].into_iter().collect(),
                    },
                    limits: ModelLimits {
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                    },
                    pricing: ModelPricing {
                        default: TokenPriceRates {
                            input: MoneyRate::new(model.input),
                            output: MoneyRate::new(model.output),
                            cache_read: MoneyRate::new(model.cache_read),
                            cache_write: MoneyRate::new(0),
                        },
                        request_wide_tiers: Vec::new(),
                        cache_write_retention: Default::default(),
                    },
                    reasoning: true,
                    headers: Default::default(),
                },
                api: match api {
                    GoogleGenerativeAi::API_ID => ApiModelConfig::GoogleGenerativeAi(config),
                    GoogleVertex::API_ID => ApiModelConfig::GoogleVertex(config),
                    _ => unreachable!("only the two pinned Google API families are valid"),
                },
                extensions: Default::default(),
            }
        })
        .collect()
}

/// Architecture v2 part 2 §1.8 and §5.1; pinned Pi basis:
/// `providers/google.ts`, `providers/google-vertex.ts`, and the generated
/// 0.84.2 `google.json` and `google-vertex.json` catalog snapshots.
#[test]
fn google_provider_catalogs_and_registrations_match_pinned_families() {
    let google = google_models().expect("Google catalog");
    let vertex = google_vertex_models().expect("Vertex catalog");
    assert_eq!(
        google,
        pinned_google_catalog_descriptors(
            PINNED_GOOGLE_CATALOG,
            "google",
            GoogleGenerativeAi::API_ID,
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        "every Google descriptor field must match the pinned Pi 0.84.2 snapshot",
    );
    // `url::Url` cannot represent Pi's `{location}` hostname template. The
    // production catalog intentionally stores the provider's default location;
    // auth resolution replaces it with the request's project/location endpoint.
    assert_eq!(
        vertex,
        pinned_google_catalog_descriptors(
            PINNED_GOOGLE_VERTEX_CATALOG,
            "google-vertex",
            GoogleVertex::API_ID,
            "https://us-central1-aiplatform.googleapis.com",
        ),
        "every Vertex descriptor field must match the pinned Pi 0.84.2 snapshot",
    );

    let google_registration =
        google_provider(std::sync::Arc::new(NeverTransport)).expect("Google registration");
    assert!(
        google_registration
            .apis
            .contains_key(&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID))
    );
    assert_eq!(
        google_registration.descriptor.headers["user-agent"].as_deref(),
        Some(google_user_agent().as_str())
    );
    assert_eq!(
        google_registration.descriptor.headers["accept"].as_deref(),
        Some("*/*")
    );
    let vertex_registration =
        google_vertex_provider(std::sync::Arc::new(NeverTransport)).expect("Vertex registration");
    assert!(
        vertex_registration
            .apis
            .contains_key(&agentprism_ai::ApiId::new(GoogleVertex::API_ID))
    );
    assert_eq!(
        vertex_registration.descriptor.headers["accept"].as_deref(),
        Some("*/*")
    );

    let local_google = local_google_provider(std::rc::Rc::new(LocalNeverTransport))
        .expect("local Google registration");
    assert!(
        local_google
            .apis
            .contains_key(&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID))
    );
    let local_vertex = local_google_vertex_provider(std::rc::Rc::new(LocalNeverTransport))
        .expect("local Vertex registration");
    assert!(
        local_vertex
            .apis
            .contains_key(&agentprism_ai::ApiId::new(GoogleVertex::API_ID))
    );

    let capture = std::sync::Arc::new(CapturingTransport::default());
    let registration = google_vertex_provider(capture.clone()).expect("capturing Vertex provider");
    let (_, mut model, context, _) = parse_fixture("google-vertex", "text-only");
    model.common.model_ref.model = ModelId::new("anthropic/claude-sonnet-4-5");
    let request = resolved_google_request(
        model,
        context,
        "https://proxy.example.com/collection",
        GoogleVertex::API_ID,
    );
    let _stream = futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleVertex::API_ID)]
            .stream(request, agentprism_ai::CancellationToken::new()),
    )
    .expect("Vertex stream establishment");
    let requests = capture.requests.lock().expect("captured requests");
    assert_eq!(
        requests[0].url.as_str(),
        "https://proxy.example.com/collection/v1/publishers/anthropic/models/claude-sonnet-4-5:streamGenerateContent?alt=sse"
    );
    assert!(requests[0].session_id.is_none());
}

/// Architecture v2 part 2 §9.2; pinned Pi basis: `google-vertex.ts:384-390`.
/// Every concrete custom Vertex base URL uses COLLECTION resource scope,
/// including custom paths hosted on a standard Google Vertex hostname.
#[test]
fn google_vertex_custom_standard_host_preserves_collection_scope_send_and_local() {
    let custom_base = "https://us-central1-aiplatform.googleapis.com/custom/vertex-collection";
    let (_, mut model, context, _) = parse_fixture("google-vertex", "text-only");
    model.common.base_url = Url::parse(custom_base).expect("custom Vertex collection URL");
    let expected = format!(
        "{custom_base}/v1/publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.common.model_ref.model
    );

    let make_send_request = || {
        let mut request = resolved_google_request(
            model.clone(),
            context.clone(),
            custom_base,
            GoogleVertex::API_ID,
        );
        request.headers.remove("x-goog-api-key");
        request.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer fixture-adc-token"),
        );
        request.api_key = None;
        request
    };
    let make_local_request = || {
        let mut request = local_resolved_google_request(
            model.clone(),
            context.clone(),
            custom_base,
            GoogleVertex::API_ID,
        );
        request.headers.remove("x-goog-api-key");
        request.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer fixture-adc-token"),
        );
        request.api_key = None;
        request
    };

    let send_capture = std::sync::Arc::new(CapturingTransport::default());
    let registration =
        google_vertex_provider(send_capture.clone()).expect("capturing Vertex provider");
    futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleVertex::API_ID)]
            .stream(make_send_request(), agentprism_ai::CancellationToken::new()),
    )
    .expect("custom collection-scoped Send request");
    assert_eq!(
        send_capture.requests.lock().expect("request lock")[0]
            .url
            .as_str(),
        expected
    );

    let local_capture = std::rc::Rc::new(CapturingTransport::default());
    let registration = local_google_vertex_provider(local_capture.clone())
        .expect("capturing local Vertex provider");
    futures_executor::block_on(agentprism_ai::LocalChatApi::stream(
        registration.apis[&agentprism_ai::ApiId::new(GoogleVertex::API_ID)].as_ref(),
        make_local_request(),
        agentprism_ai::CancellationToken::new(),
    ))
    .expect("custom collection-scoped Local request");
    assert_eq!(
        local_capture.requests.lock().expect("request lock")[0]
            .url
            .as_str(),
        expected
    );
}

/// Pinned Pi `google-generative-ai.ts` delegates model normalization to
/// `@google/genai` 1.52.0, whose `tModel` preserves both qualified Developer
/// API resource forms. This exercises the rule through both runtime families.
#[test]
fn google_generative_ai_qualified_model_urls_pi_exact_send_and_local() {
    for (model_id, expected_path) in [
        (
            "models/gemini-qualified",
            "/v1beta/models/gemini-qualified:streamGenerateContent",
        ),
        (
            "tunedModels/123456789",
            "/v1beta/tunedModels/123456789:streamGenerateContent",
        ),
    ] {
        let (_, mut model, context, _) = parse_fixture("google-generative-ai", "text-only");
        model.common.model_ref.model = ModelId::new(model_id);

        let send_capture = std::sync::Arc::new(CapturingTransport::default());
        let registration =
            google_provider(send_capture.clone()).expect("capturing Google provider");
        let request = resolved_google_request(
            model.clone(),
            context.clone(),
            "https://generativelanguage.googleapis.com/v1beta",
            GoogleGenerativeAi::API_ID,
        );
        futures_executor::block_on(
            registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)]
                .stream(request, agentprism_ai::CancellationToken::new()),
        )
        .expect("qualified Send request");
        assert_eq!(
            send_capture.requests.lock().expect("request lock")[0]
                .url
                .path(),
            expected_path
        );

        let local_capture = std::rc::Rc::new(CapturingTransport::default());
        let registration =
            local_google_provider(local_capture.clone()).expect("capturing local Google provider");
        let request = local_resolved_google_request(
            model,
            context,
            "https://generativelanguage.googleapis.com/v1beta",
            GoogleGenerativeAi::API_ID,
        );
        futures_executor::block_on(agentprism_ai::LocalChatApi::stream(
            registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)].as_ref(),
            request,
            agentprism_ai::CancellationToken::new(),
        ))
        .expect("qualified Local request");
        assert_eq!(
            local_capture.requests.lock().expect("request lock")[0]
                .url
                .path(),
            expected_path
        );
    }
}

/// Architecture v2 part 2 §3.3 and §9.2; pinned Pi basis:
/// `google-generative-ai.ts:431-463`, whose Gemini 3 regex is deliberately
/// unanchored and therefore recognizes qualified `models/...` identifiers.
#[test]
fn google_qualified_gemini3_disabled_thinking_pi_exact_send_and_local() {
    let (_, mut model, context, _) = parse_fixture("google-generative-ai", "text-only");
    model.common.model_ref.model = ModelId::new("models/gemini-3.1-pro-preview");

    let send_capture = std::sync::Arc::new(CapturingTransport::default());
    let registration = google_provider(send_capture.clone()).expect("capturing Google provider");
    futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)].stream(
            resolved_google_request(
                model.clone(),
                context.clone(),
                "https://generativelanguage.googleapis.com/v1beta",
                GoogleGenerativeAi::API_ID,
            ),
            agentprism_ai::CancellationToken::new(),
        ),
    )
    .expect("qualified Send request");
    let send_body: Value =
        serde_json::from_slice(&send_capture.requests.lock().expect("Send request lock")[0].body)
            .expect("Send body JSON");
    assert_eq!(thinking_config(&send_body).unwrap()["thinkingLevel"], "LOW");
    assert!(
        thinking_config(&send_body)
            .unwrap()
            .get("includeThoughts")
            .is_none()
    );

    let local_capture = std::rc::Rc::new(CapturingTransport::default());
    let registration =
        local_google_provider(local_capture.clone()).expect("capturing local Google provider");
    futures_executor::block_on(agentprism_ai::LocalChatApi::stream(
        registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)].as_ref(),
        local_resolved_google_request(
            model,
            context,
            "https://generativelanguage.googleapis.com/v1beta",
            GoogleGenerativeAi::API_ID,
        ),
        agentprism_ai::CancellationToken::new(),
    ))
    .expect("qualified Local request");
    let local_body: Value =
        serde_json::from_slice(&local_capture.requests.lock().expect("Local request lock")[0].body)
            .expect("Local body JSON");
    assert_eq!(
        thinking_config(&local_body).unwrap()["thinkingLevel"],
        "LOW"
    );
    assert!(
        thinking_config(&local_body)
            .unwrap()
            .get("includeThoughts")
            .is_none()
    );
}

/// Pinned `@google/genai` 1.52.0 `tModel` rejects the empty model and model
/// strings containing `..`, `?`, or `&` before transport.
#[test]
fn google_invalid_sdk_model_syntax_rejected_pi_exact_send_and_local() {
    for model_id in ["", "models/../secret", "gemini?alt=json", "gemini&x=y"] {
        let (_, mut model, context, _) = parse_fixture("google-generative-ai", "text-only");
        model.common.model_ref.model = ModelId::new(model_id);

        let send_capture = std::sync::Arc::new(CapturingTransport::default());
        let registration =
            google_provider(send_capture.clone()).expect("capturing Google provider");
        let request = resolved_google_request(
            model.clone(),
            context.clone(),
            "https://generativelanguage.googleapis.com/v1beta",
            GoogleGenerativeAi::API_ID,
        );
        let error = futures_executor::block_on(
            registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)]
                .stream(request, agentprism_ai::CancellationToken::new()),
        )
        .expect_err("invalid Send model must fail");
        assert_eq!(error.kind, agentprism_ai::AiErrorKind::Transport);
        assert!(
            send_capture
                .requests
                .lock()
                .expect("request lock")
                .is_empty()
        );

        let local_capture = std::rc::Rc::new(CapturingTransport::default());
        let registration =
            local_google_provider(local_capture.clone()).expect("capturing local Google provider");
        let request = local_resolved_google_request(
            model,
            context,
            "https://generativelanguage.googleapis.com/v1beta",
            GoogleGenerativeAi::API_ID,
        );
        let error = futures_executor::block_on(agentprism_ai::LocalChatApi::stream(
            registration.apis[&agentprism_ai::ApiId::new(GoogleGenerativeAi::API_ID)].as_ref(),
            request,
            agentprism_ai::CancellationToken::new(),
        ))
        .expect_err("invalid Local model must fail");
        assert_eq!(error.kind, agentprism_ai::AiErrorKind::Transport);
        assert!(
            local_capture
                .requests
                .lock()
                .expect("request lock")
                .is_empty()
        );
    }
}

/// Architecture v2 part 1 §3.4 and part 2 §9.2; pinned Pi basis:
/// `google-vertex.ts:46-55,437-455`.
#[test]
fn google_vertex_full_options_project_location_precedence_pi_exact_send_and_local() {
    use futures_util::StreamExt as _;

    let (_, model, context, _) = parse_fixture("google-vertex", "text-only");
    let options = GoogleVertexOptions {
        project: Some("per-call-project".to_owned()),
        location: Some("eu".to_owned()),
        ..GoogleVertexOptions::default()
    };
    let expected = "https://aiplatform.eu.rep.googleapis.com/v1/projects/per-call-project/locations/eu/publishers/google/models/gemini-3-fixture:streamGenerateContent?alt=sse";

    let make_request = || {
        let mut request = resolved_google_request(
            model.clone(),
            context.clone(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/environment-project/locations/us-central1",
            GoogleVertex::API_ID,
        );
        request.headers.remove("x-goog-api-key");
        request.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer fixture-adc-token"),
        );
        request.full_options = Some(agentprism_ai::ErasedApiFullOptions::new::<GoogleVertex>(
            options.clone(),
        ));
        request
    };
    let make_local_request = || {
        let mut request = local_resolved_google_request(
            model.clone(),
            context.clone(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/environment-project/locations/us-central1",
            GoogleVertex::API_ID,
        );
        request.headers.remove("x-goog-api-key");
        request.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer fixture-adc-token"),
        );
        request.full_options = Some(agentprism_ai::ErasedApiFullOptions::new::<GoogleVertex>(
            options.clone(),
        ));
        request
    };

    let send_capture = std::sync::Arc::new(CapturingTransport::default());
    let registration =
        google_vertex_provider(send_capture.clone()).expect("capturing Vertex provider");
    futures_executor::block_on(
        registration.apis[&agentprism_ai::ApiId::new(GoogleVertex::API_ID)]
            .stream(make_request(), agentprism_ai::CancellationToken::new()),
    )
    .expect("Send full Vertex request");
    assert_eq!(
        send_capture.requests.lock().expect("request lock")[0]
            .url
            .as_str(),
        expected
    );

    let local_capture = std::rc::Rc::new(CapturingTransport::default());
    let registration = local_google_vertex_provider(local_capture.clone())
        .expect("capturing local Vertex provider");
    futures_executor::block_on(agentprism_ai::LocalChatApi::stream(
        registration.apis[&agentprism_ai::ApiId::new(GoogleVertex::API_ID)].as_ref(),
        make_local_request(),
        agentprism_ai::CancellationToken::new(),
    ))
    .expect("Local full Vertex request");
    assert_eq!(
        local_capture.requests.lock().expect("request lock")[0]
            .url
            .as_str(),
        expected
    );

    // The ADC file is the only ambient auth input. Typed full options provide
    // the project/location needed during auth and retain their later route
    // precedence in both runtime families.
    let adc_document = r#"{
        "type":"authorized_user",
        "client_id":"fixture-client",
        "client_secret":"fixture-secret",
        "refresh_token":"fixture-refresh",
        "token_uri":"https://oauth2.googleapis.com/token"
    }"#;
    let auth_context =
        agentprism_ai::MapAuthContext::new(std::collections::BTreeMap::new(), Vec::<String>::new())
            .with_file(
                agentprism_google_vertex::VERTEX_ADC_PATH,
                agentprism_ai::SecretString::new(adc_document),
            );

    let send_transport = std::sync::Arc::new(AdcTransport::default());
    let model_ref = google_vertex_models().expect("Vertex catalog")[0]
        .common
        .model_ref
        .clone();
    let model_id = model_ref.model.clone();
    let models = Models::builder()
        .auth_context(std::sync::Arc::new(auth_context.clone()))
        .provider(google_vertex_provider(send_transport.clone()).expect("Send Vertex registration"))
        .build()
        .expect("Send Vertex Models");
    let stream = futures_executor::block_on(models.stream_api::<GoogleVertex>(
        model_ref,
        Context::new(None),
        options.clone(),
        agentprism_ai::CancellationToken::new(),
    ))
    .expect("full Vertex options override valid resolved Send scope");
    let _events = futures_executor::block_on(stream.collect::<Vec<_>>());
    let requests = send_transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].url.as_str(),
        format!(
            "https://aiplatform.eu.rep.googleapis.com/v1/projects/per-call-project/locations/eu/publishers/google/models/{model_id}:streamGenerateContent?alt=sse"
        )
    );

    let local_transport = std::rc::Rc::new(AdcTransport::default());
    let model_ref = google_vertex_models().expect("local Vertex catalog")[0]
        .common
        .model_ref
        .clone();
    let models = agentprism_ai::LocalModels::builder()
        .auth_context(std::rc::Rc::new(auth_context))
        .provider(
            local_google_vertex_provider(local_transport.clone())
                .expect("Local Vertex registration"),
        )
        .build()
        .expect("Local Vertex Models");
    let stream = futures_executor::block_on(models.stream_api::<GoogleVertex>(
        model_ref,
        Context::new(None),
        options,
        agentprism_ai::CancellationToken::new(),
    ))
    .expect("full Vertex options override valid resolved Local scope");
    let _events = futures_executor::block_on(stream.collect::<Vec<_>>());
    let requests = local_transport.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .url
            .as_str()
            .contains("/v1/projects/per-call-project/locations/eu/publishers/google/models/")
    );
}
