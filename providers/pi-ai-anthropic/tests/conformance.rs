use futures_util::StreamExt;
use http::HeaderMap;
use pi_ai::{
    ANTHROPIC_REDACTED_THINKING_KIND, ANTHROPIC_THINKING_SIGNATURE_KIND, AnthropicEffort,
    AnthropicMessages, AnthropicMessagesCompat, AnthropicMessagesHandoff,
    AnthropicMessagesModelConfig, AnthropicOptions, AnthropicSimplePatch, AnthropicThinking,
    AnthropicThinkingDisplay, AnthropicToolChoice, ApiFamily, ApiModelConfig, ApiRequestOptions,
    AssistantEvent, AssistantFinish, AssistantFinishReason, AssistantMessage, AuthAnswer,
    AuthEvent, AuthHostCapabilities, AuthInteraction, AuthInteractionError, AuthPrompt,
    CacheRetention, CacheWriteRetention, CancellationToken, CommonModelDescriptor,
    ConstrainedSampling, ConstrainedSamplingConfig, ContentBlock, ContentBlockId, Context,
    Credential, Currency, EncodeContext, ErasedPayloadContext, ErasedPayloadTransform,
    HeaderTransform, HeaderTransformContext, HttpRequest, HttpResponse, HttpTransport,
    InMemoryCredentialStore, JsonSchemaStrictMode, LevelSupport, LocalBoxFuture,
    LocalErasedPayloadTransform, LocalHeaderTransform, LocalHttpResponse, LocalHttpTransport,
    LocalModelRuntime, LocalModels, LocalProviderRegistration, MapAuthContext, Message, MessageId,
    MiddlewareError, Modality, ModalityCapabilities, ModelDescriptor, ModelId, ModelLimits,
    ModelPricing, ModelRef, ModelRequest, ModelRuntime, Models, OAuthAuth, OAuthCredential,
    OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter, PayloadTransformDisposition,
    ProviderId, ProviderOAuthExtra, ProviderPayload, ProviderRegistration, ReasoningLevel,
    RedirectReceiver, RedirectReceiverRequest, ReplayApplicability, ReplayCompleteness,
    ReplayEnvelope, ReplayItem, ReplayItemId, ReplayKind, ReplayScope, ReplayTarget,
    RequestStartErrorKind, ResolveAuthRequest, SecretString, SendBoxFuture,
    SimpleGenerationOptions, SimpleLoweringContext, ThinkingLevelMap, Timestamp, TokenPriceRates,
    ToolCall, ToolCallId, ToolChoice, ToolResultContent, ToolResultMessage, ToolSpec,
    TransportError, TypedModelDescriptor, Usage, UsageSource, estimate_context_tokens,
    transform_context_for_model,
};
use pi_ai_anthropic::{
    AnthropicMessagesDecodeContext, AnthropicMessagesSseDecoder, AnthropicOAuth,
    anthropic_default_headers, anthropic_messages_api, anthropic_models, anthropic_provider,
    anthropic_user_agent, decode_anthropic_messages_sse, local_anthropic_messages_api,
    local_anthropic_provider,
};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use url::Url;

const FIXTURE_TIMESTAMP: i64 = 1_700_000_000_000;
const FIXTURE_API_KEY: &str = "fixture-secret-never-captured";

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/anthropic-messages")
}

fn case_names() -> Vec<String> {
    let mut names = fs::read_dir(fixture_root())
        .expect("fixture root")
        .map(|entry| {
            entry
                .expect("fixture directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn read_value(case: &str, file: &str) -> Value {
    serde_json::from_slice(&fs::read(fixture_root().join(case).join(file)).expect("fixture file"))
        .expect("fixture JSON")
}

fn read_bytes(case: &str, file: &str) -> Vec<u8> {
    fs::read(fixture_root().join(case).join(file)).expect("fixture bytes")
}

fn parse_fixture(case: &str) -> (Value, ModelDescriptor, Context, SimpleGenerationOptions) {
    let fixture = read_value(case, "canonical.json");
    let model = parse_model(&fixture["model"]);
    let context = parse_context(&fixture["context"], &model);
    let simple = parse_simple(&fixture["options"]);
    (fixture, model, context, simple)
}

fn parse_model(value: &Value) -> ModelDescriptor {
    let provider = value["provider"].as_str().expect("provider");
    let model_id = value["id"].as_str().expect("model id");
    let input = value["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(parse_modality)
        .collect::<BTreeSet<_>>();
    let mut output = BTreeSet::new();
    output.insert(Modality::Text);
    let compatibility = parse_compat(&value["compat"]);
    let thinking_levels = parse_thinking_levels(&value["thinkingLevelMap"]);
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, model_id),
            display_name: value["name"].as_str().unwrap_or(model_id).to_owned(),
            base_url: Url::parse("http://127.0.0.1:1/v1").expect("fixture URL"),
            modalities: ModalityCapabilities { input, output },
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
            headers: value["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.clone(), Some(value.to_owned())))
                })
                .collect(),
        },
        api: ApiModelConfig::AnthropicMessages(AnthropicMessagesModelConfig {
            compat: compatibility,
            thinking_levels,
        }),
        extensions: Default::default(),
    }
}

fn parse_modality(value: &str) -> Option<Modality> {
    match value {
        "text" => Some(Modality::Text),
        "image" => Some(Modality::Image),
        "audio" => Some(Modality::Audio),
        _ => None,
    }
}

fn parse_compat(value: &Value) -> AnthropicMessagesCompat {
    let get = |name| value.get(name).and_then(Value::as_bool);
    AnthropicMessagesCompat {
        supports_eager_tool_input_streaming: get("supportsEagerToolInputStreaming"),
        supports_long_cache_retention: get("supportsLongCacheRetention"),
        send_session_affinity_headers: get("sendSessionAffinityHeaders"),
        supports_cache_control_on_tools: get("supportsCacheControlOnTools"),
        supports_temperature: get("supportsTemperature"),
        force_adaptive_thinking: get("forceAdaptiveThinking"),
        allow_empty_signature: get("allowEmptySignature"),
        supports_strict_tools: get("supportsStrictTools"),
        supports_tool_references: get("supportsToolReferences"),
        allowed_fallback_models: Vec::new(),
        extensions: Default::default(),
    }
}

fn parse_thinking_levels(value: &Value) -> ThinkingLevelMap<pi_ai::AnthropicThinkingValue> {
    let parse = |name: &str| {
        value.get(name).map(|value| match value.as_str() {
            None => LevelSupport::Unsupported,
            Some("off") => LevelSupport::Value(pi_ai::AnthropicThinkingValue::Off),
            Some("minimal") => LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(
                AnthropicEffort::Minimal,
            )),
            Some("low") => {
                LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(AnthropicEffort::Low))
            }
            Some("medium") => LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(
                AnthropicEffort::Medium,
            )),
            Some("high") => {
                LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(AnthropicEffort::High))
            }
            Some("xhigh") => LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(
                AnthropicEffort::Xhigh,
            )),
            Some("max") => {
                LevelSupport::Value(pi_ai::AnthropicThinkingValue::Effort(AnthropicEffort::Max))
            }
            Some(other) => panic!("unknown Anthropic thinking mapping {other}"),
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
        max_retries: value
            .get("maxRetries")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        timeout_ms: value.get("timeoutMs").and_then(Value::as_u64),
        max_output_tokens: value
            .get("maxTokens")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|v| v as f32),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_str)
            .map(parse_reasoning),
        sampling,
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        headers: value
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value.as_str().map(std::borrow::ToOwned::to_owned),
                )
            })
            .collect(),
        cache_retention: value.get("cacheRetention").and_then(Value::as_str).map(
            |value| match value {
                "none" => CacheRetention::None,
                "short" => CacheRetention::Short,
                "long" => CacheRetention::Long,
                other => panic!("unknown cache retention {other}"),
            },
        ),
        tool_choice: value
            .get("toolChoice")
            .and_then(Value::as_str)
            .map(|value| match value {
                "auto" => ToolChoice::Auto,
                "none" => ToolChoice::None,
                other => panic!("unsupported fixture tool choice {other}"),
            }),
        ..SimpleGenerationOptions::default()
    }
}

fn parse_reasoning(value: &str) -> ReasoningLevel {
    match value {
        "minimal" => ReasoningLevel::Minimal,
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::Xhigh,
        "max" => ReasoningLevel::Max,
        other => panic!("unknown reasoning level {other}"),
    }
}

fn parse_context(value: &Value, default_model: &ModelDescriptor) -> Context {
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
            .map(|(index, value)| parse_message(value, index, default_model))
            .collect(),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(parse_tool)
            .collect(),
    }
}

fn parse_message(value: &Value, index: usize, default_model: &ModelDescriptor) -> Message {
    let timestamp =
        Timestamp::from_unix_millis(value["timestamp"].as_i64().unwrap_or(FIXTURE_TIMESTAMP));
    match value["role"].as_str().expect("message role") {
        "user" => Message::User(pi_ai::UserMessage {
            id: MessageId::new(format!("fixture-user-{index}")),
            content: parse_user_content(&value["content"], index),
            timestamp,
        }),
        "toolResult" => Message::ToolResult(ToolResultMessage {
            id: MessageId::new(format!("fixture-tool-result-{index}")),
            tool_call_id: ToolCallId::new(value["toolCallId"].as_str().expect("tool call id")),
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
        "assistant" => Message::Assistant(parse_assistant(value, index, default_model)),
        other => panic!("unknown message role {other}"),
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

fn parse_assistant(
    value: &Value,
    index: usize,
    default_model: &ModelDescriptor,
) -> AssistantMessage {
    let provider = value["provider"]
        .as_str()
        .unwrap_or(default_model.common.model_ref.provider.as_str());
    let api = value["api"].as_str().unwrap_or("anthropic-messages");
    let model = value["model"]
        .as_str()
        .unwrap_or(default_model.common.model_ref.model.as_str());
    let message_id = MessageId::new(format!("fixture-assistant-{index}"));
    let mut replay = ReplayEnvelope::new(ReplayScope::new(provider, api, model, model));
    let mut content = Vec::new();
    for (block_index, block) in value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = ContentBlockId::new(format!("fixture-assistant-{index}-{block_index}"));
        match block["type"].as_str() {
            Some("text") => content.push(ContentBlock::Text {
                id,
                text: block["text"].as_str().unwrap_or_default().to_owned(),
            }),
            Some("thinking") => {
                let redacted = block["redacted"].as_bool().unwrap_or(false);
                if let Some(signature) = block["thinkingSignature"].as_str() {
                    let replay_id =
                        ReplayItemId::new(format!("fixture-replay-{index}-{block_index}"));
                    replay.items.push(ReplayItem {
                        id: replay_id.clone(),
                        ordinal: u32::try_from(block_index).expect("replay ordinal"),
                        target: ReplayTarget::ContentBlock(id.clone()),
                        kind: ReplayKind::new(if redacted {
                            ANTHROPIC_REDACTED_THINKING_KIND
                        } else {
                            ANTHROPIC_THINKING_SIGNATURE_KIND
                        }),
                        applicability: ReplayApplicability::ExactProviderApiModel,
                        completeness: ReplayCompleteness::Complete,
                        payload: pi_ai::OpaquePayload::Utf8(signature.to_owned()),
                    });
                    content.push(ContentBlock::Thinking {
                        id,
                        text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                        redacted,
                        replay_item: Some(replay_id),
                    });
                } else {
                    content.push(ContentBlock::Thinking {
                        id,
                        text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                        redacted,
                        replay_item: None,
                    });
                }
            }
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
    let raw_reason = value["stopReason"].as_str().unwrap_or("stop");
    let reason = match raw_reason {
        "stop" => AssistantFinishReason::Stop,
        "length" => AssistantFinishReason::Length,
        "toolUse" => AssistantFinishReason::ToolUse,
        "error" => AssistantFinishReason::Error,
        "aborted" => AssistantFinishReason::Aborted,
        other => panic!("unknown fixture stop reason {other}"),
    };
    AssistantMessage {
        id: message_id,
        provider: ProviderId::new(provider),
        api: pi_ai::ApiId::new(api),
        requested_model: ModelId::new(model),
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
            error: matches!(
                reason,
                AssistantFinishReason::Error | AssistantFinishReason::Aborted
            )
            .then(|| pi_ai::PublicError {
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
    let constrained_sampling = value.get("constrainedSampling").map(|value| {
        ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema {
            strict: match value["strict"].as_str().unwrap_or("prefer") {
                "prefer" => JsonSchemaStrictMode::Prefer,
                "require" => JsonSchemaStrictMode::Require,
                other => panic!("unknown strict preference {other}"),
            },
        })
    });
    ToolSpec {
        schema_version: 1,
        name: value["name"].as_str().expect("tool name").to_owned(),
        description: value["description"].as_str().unwrap_or_default().to_owned(),
        parameters: value["parameters"].clone(),
        constrained_sampling,
    }
}

fn typed_model(model: &ModelDescriptor) -> TypedModelDescriptor<AnthropicMessages> {
    let ApiModelConfig::AnthropicMessages(config) = &model.api else {
        panic!("fixture model API")
    };
    TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    }
}

fn encode_fixture(
    entrypoint: &str,
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
) -> Vec<u8> {
    let typed = typed_model(model);
    let ApiModelConfig::AnthropicMessages(config) = &model.api else {
        unreachable!()
    };
    let compatibility = AnthropicMessages::resolve_compat(&model.common.base_url, &config.compat)
        .expect("resolve compatibility");
    let options = if entrypoint == "streamSimple" {
        let (estimated_input_tokens, available_context_tokens) =
            if model.common.limits.context_window == 0 {
                (0, 0)
            } else {
                let estimate = estimate_context_tokens(context).expect("estimate context");
                let available = model
                    .common
                    .limits
                    .context_window
                    .saturating_sub(estimate.tokens)
                    .saturating_sub(pi_ai::CONTEXT_SAFETY_TOKENS);
                (estimate.tokens, available)
            };
        AnthropicMessages::lower_simple(
            SimpleLoweringContext {
                model: &typed,
                compat: &compatibility,
                effective_base_url: &model.common.base_url,
                estimated_input_tokens,
                available_context_tokens,
            },
            simple,
            &AnthropicSimplePatch::default(),
        )
        .expect("lower simple")
    } else {
        full_options_from_simple(model, simple)
    };
    let projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &AnthropicMessagesHandoff,
    )
    .expect("project context")
    .context;
    let request = AnthropicMessages::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compatibility,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("encode request");
    OrderedJsonWriter::to_vec(&request.into()).expect("write ordered request")
}

fn full_options_from_simple(
    model: &ModelDescriptor,
    simple: &SimpleGenerationOptions,
) -> AnthropicOptions {
    AnthropicOptions {
        max_tokens: simple
            .max_output_tokens
            .unwrap_or(model.common.limits.max_output_tokens),
        temperature: simple.temperature,
        thinking: AnthropicThinking::Omitted,
        thinking_display: AnthropicThinkingDisplay::Summarized,
        tool_choice: simple.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => AnthropicToolChoice::Auto,
            ToolChoice::None => AnthropicToolChoice::None,
        }),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        metadata_user_id: None,
        interleaved_thinking: true,
    }
}

fn decode_turn_one(case: &str, model: &ModelDescriptor) -> AssistantMessage {
    let events = decode_anthropic_messages_sse(
        &read_bytes(case, "response-turn-1.sse"),
        AnthropicMessagesDecodeContext {
            message_id: MessageId::new(format!("fixture-response-{case}")),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
            tool_name_aliases: BTreeMap::new(),
        },
    );
    events
        .into_iter()
        .find_map(|event| match event {
            AssistantEvent::Finished { message } => Some(message),
            AssistantEvent::Failed { message } => {
                panic!("fixture decode failed: {:?}", message.finish.error)
            }
            _ => None,
        })
        .expect("terminal response")
}

fn encode_turn_two(case: &str) -> Vec<u8> {
    let (fixture, model, mut context, simple) = parse_fixture(case);
    let output = decode_turn_one(case, &model);
    let persisted = serde_json::to_vec(&output).expect("persist assistant");
    context.messages.push(Message::Assistant(
        serde_json::from_slice(&persisted).expect("restore assistant"),
    ));
    for (offset, message) in fixture["turnTwoAppend"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        context.messages.push(parse_message(
            message,
            context.messages.len() + offset,
            &model,
        ));
    }
    encode_fixture(
        fixture["entrypoint"].as_str().expect("entrypoint"),
        &model,
        &context,
        &simple,
    )
}

fn assert_fixture_pipeline(
    case: &str,
    fixture: &Value,
    model: &ModelDescriptor,
    mut context: Context,
    simple: &SimpleGenerationOptions,
) {
    let response = read_bytes(case, "response-turn-1.sse");
    let transport = Arc::new(FixturePipelineTransport::new([response.clone(), response]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("Anthropic fixture registration");
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("Anthropic fixture Models");

    let entrypoint = fixture["entrypoint"].as_str().expect("entrypoint");
    let first_events = run_fixture_pipeline_entrypoint(
        &models,
        model,
        context.clone(),
        simple.clone(),
        entrypoint,
    );
    let first = first_events
        .into_iter()
        .find_map(|event| match event {
            AssistantEvent::Finished { message } => Some(message),
            AssistantEvent::Failed { message } => {
                panic!(
                    "Anthropic fixture pipeline failed: {:?}",
                    message.finish.error
                )
            }
            _ => None,
        })
        .expect("Anthropic fixture pipeline terminal");
    let persisted = serde_json::to_vec(&first).expect("persist pipeline assistant");
    context.messages.push(Message::Assistant(
        serde_json::from_slice(&persisted).expect("restore pipeline assistant"),
    ));
    let first_index = context.messages.len();
    for (offset, message) in fixture["turnTwoAppend"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        context
            .messages
            .push(parse_message(message, first_index + offset, model));
    }
    let _second_events =
        run_fixture_pipeline_entrypoint(&models, model, context, simple.clone(), entrypoint);

    let requests = transport.requests.lock().expect("fixture request lock");
    assert_eq!(requests.len(), 2, "{case} must send both fixture turns");
    assert_request_capture(
        case,
        1,
        &requests[0],
        Some(read_bytes(case, "request-turn-1.body.json")),
    );
    assert_request_capture(
        case,
        2,
        &requests[1],
        Some(read_bytes(case, "request-turn-2.body.json")),
    );
}

fn run_fixture_pipeline_entrypoint(
    models: &Models,
    model: &ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
    entrypoint: &str,
) -> Vec<AssistantEvent> {
    if entrypoint == "streamSimple" {
        run_fixture_pipeline(models, model, context, options)
    } else {
        run_full_fixture_pipeline(models, model, context, options)
    }
}

fn run_fixture_pipeline(
    models: &Models,
    model: &ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
) -> Vec<AssistantEvent> {
    let mut stream = futures_executor::block_on(ModelRuntime::stream(
        models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context,
            options,
        },
        CancellationToken::new(),
    ))
    .expect("Anthropic fixture stream establishment");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn run_full_fixture_pipeline(
    models: &Models,
    model: &ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
) -> Vec<AssistantEvent> {
    let full_options = full_options_from_simple(model, &options);
    let request_options = ApiRequestOptions::from(&options);
    let mut stream =
        futures_executor::block_on(models.stream_api_with_request_options::<AnthropicMessages>(
            model.common.model_ref.clone(),
            context,
            full_options,
            request_options,
            CancellationToken::new(),
        ))
        .expect("Anthropic full-options fixture stream establishment");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn run_registered_full_options(
    models: &Models,
    model: &ModelDescriptor,
    context: Context,
    options: AnthropicOptions,
) -> Vec<AssistantEvent> {
    let mut stream = futures_executor::block_on(models.stream_api::<AnthropicMessages>(
        model.common.model_ref.clone(),
        context,
        options,
        CancellationToken::new(),
    ))
    .expect("registered Anthropic full-options stream establishment");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn run_registered_local_full_options(
    models: &LocalModels,
    model: &ModelDescriptor,
    context: Context,
    options: AnthropicOptions,
) -> Vec<AssistantEvent> {
    let mut stream = futures_executor::block_on(models.stream_api::<AnthropicMessages>(
        model.common.model_ref.clone(),
        context,
        options,
        CancellationToken::new(),
    ))
    .expect("registered local Anthropic full-options stream establishment");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn run_local_fixture_pipeline(
    models: &LocalModels,
    model: &ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
) -> Vec<AssistantEvent> {
    let mut stream = futures_executor::block_on(LocalModelRuntime::stream(
        models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context,
            options,
        },
        CancellationToken::new(),
    ))
    .expect("local Anthropic fixture stream establishment");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn assert_request_capture(
    case: &str,
    turn: u8,
    request: &HttpRequest,
    expected_body: Option<Vec<u8>>,
) {
    let capture = read_value(case, &format!("request-turn-{turn}.headers.json"));
    assert_eq!(capture["schemaVersion"], 1);
    assert_eq!(
        request.method.as_str(),
        capture["method"].as_str().expect("captured method"),
        "method mismatch for {case} turn {turn}"
    );
    assert_eq!(
        request.url.path(),
        capture["path"].as_str().expect("captured path"),
        "path mismatch for {case} turn {turn}"
    );
    assert_eq!(
        request.url.query(),
        capture.get("query").and_then(Value::as_str),
        "query mismatch for {case} turn {turn}"
    );
    if let Some(expected_body) = expected_body {
        assert_eq!(
            request.body, expected_body,
            "pipeline body mismatch for {case} turn {turn}"
        );
    }

    let omitted = capture["omittedRuntimeHeaders"]
        .as_array()
        .expect("omitted runtime headers")
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    for name in &omitted {
        if *name != "user-agent" {
            assert!(
                request.headers.get(*name).is_none(),
                "runtime header {name} was not omitted for {case} turn {turn}"
            );
        }
    }
    let expected_user_agent = anthropic_user_agent();
    assert_eq!(
        request
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some(expected_user_agent.as_str()),
        "Pi User-Agent mismatch for {case} turn {turn}"
    );

    let mut actual_headers = BTreeMap::new();
    for (name, value) in &request.headers {
        if omitted.contains(name.as_str()) {
            continue;
        }
        actual_headers.insert(
            name.as_str().to_owned(),
            if is_sensitive_header(name.as_str()) {
                "[REDACTED]".to_owned()
            } else {
                value.to_str().expect("fixture header UTF-8").to_owned()
            },
        );
    }
    let expected_headers = capture["headers"]
        .as_object()
        .expect("captured headers")
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value.as_str().expect("captured header string").to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        actual_headers, expected_headers,
        "logical header mismatch for {case} turn {turn}"
    );
    assert_eq!(
        request.headers.get("x-api-key").expect("raw API key"),
        FIXTURE_API_KEY
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains(FIXTURE_API_KEY));
    assert!(debug.to_ascii_lowercase().contains("redacted"));
    assert_eq!(read_value(case, "metadata.json")["secretsRedacted"], true);
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

struct FixturePipelineTransport {
    responses: Mutex<VecDeque<Vec<u8>>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FixturePipelineTransport {
    fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl HttpTransport for FixturePipelineTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.requests
            .lock()
            .expect("fixture request lock")
            .push(request);
        let response = self
            .responses
            .lock()
            .expect("fixture response lock")
            .pop_front()
            .expect("fixture response");
        Box::pin(async move { Ok(HttpResponse::from_bytes(200, HeaderMap::new(), response)) })
    }
}

struct LocalFixturePipelineTransport {
    responses: RefCell<VecDeque<Vec<u8>>>,
    requests: RefCell<Vec<HttpRequest>>,
}

impl LocalFixturePipelineTransport {
    fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            responses: RefCell::new(responses.into_iter().collect()),
            requests: RefCell::new(Vec::new()),
        }
    }
}

impl LocalHttpTransport for LocalFixturePipelineTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.requests.borrow_mut().push(request);
        let response = self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("local fixture response");
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                response,
            ))
        })
    }
}

struct ReplaceAnthropicPayloadWithStreamingDisabled;

impl ErasedPayloadTransform for ReplaceAnthropicPayloadWithStreamingDisabled {
    fn transform<'a>(
        &'a self,
        _context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> SendBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async {
            Ok(PayloadTransformDisposition::Replace(ProviderPayload::json(
                br#"{"replacement":true,"stream":false}"#.to_vec(),
            )))
        })
    }
}

impl LocalErasedPayloadTransform for ReplaceAnthropicPayloadWithStreamingDisabled {
    fn transform<'a>(
        &'a self,
        _context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async {
            Ok(PayloadTransformDisposition::Replace(ProviderPayload::json(
                br#"{"replacement":true,"stream":false}"#.to_vec(),
            )))
        })
    }
}

const ANTHROPIC_DEFAULT_HEADER_NAMES: [&str; 7] = [
    "user-agent",
    "anthropic-beta",
    "x-session-affinity",
    "accept",
    "content-type",
    "anthropic-version",
    "anthropic-dangerous-direct-browser-access",
];

struct AssertAnthropicDefaultsAbsent;

impl HeaderTransform for AssertAnthropicDefaultsAbsent {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
                assert!(
                    !headers.contains_key(name),
                    "{name} must be absent before the final header transform"
                );
            }
            Ok(())
        })
    }
}

struct DeleteAnthropicDefaults;

impl HeaderTransform for DeleteAnthropicDefaults {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
                headers.remove(name);
            }
            Ok(())
        })
    }
}

struct DeleteAnthropicAuthHeaders;

impl HeaderTransform for DeleteAnthropicAuthHeaders {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        delete_anthropic_auth_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

struct LocalDeleteAnthropicAuthHeaders;

impl LocalHeaderTransform for LocalDeleteAnthropicAuthHeaders {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        delete_anthropic_auth_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

fn delete_anthropic_auth_headers(headers: &mut HeaderMap) {
    for name in ["authorization", "x-api-key", "cf-aig-authorization"] {
        headers.remove(name);
    }
}

#[derive(Default)]
struct CountingTransport {
    calls: AtomicUsize,
}

impl HttpTransport for CountingTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(TransportError::new(
                "unexpected_transport",
                "transport must not execute without final Anthropic auth",
            ))
        })
    }
}

#[derive(Default)]
struct LocalCountingTransport {
    calls: Cell<usize>,
}

impl LocalHttpTransport for LocalCountingTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.calls.set(self.calls.get() + 1);
        Box::pin(async {
            Err(TransportError::new(
                "unexpected_transport",
                "transport must not execute without final Anthropic auth",
            ))
        })
    }
}

fn capture_anthropic_headers(
    model: ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
    transforms: Vec<Arc<dyn HeaderTransform>>,
) -> HeaderMap {
    let response = read_bytes("text-only", "response-turn-1.sse");
    let transport = Arc::new(FixturePipelineTransport::new([response]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("Anthropic header-test registration");
    let mut builder = Models::builder().provider(registration);
    for transform in transforms {
        builder = builder.header_transform(transform);
    }
    let models = builder.build().expect("Anthropic header-test Models");
    let _ = run_fixture_pipeline(&models, &model, context, options);
    transport.requests.lock().expect("header requests")[0]
        .headers
        .clone()
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:880-970` builds defaults before
/// `model.headers`, so a model deletion removes both static and dynamic values.
#[test]
fn headers_model_before_explicit() {
    let (_, mut model, context, mut options) = parse_fixture("text-only");
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.send_session_affinity_headers = Some(true);
    options.session_id = Some("model-delete-session".to_owned());
    for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
        model.common.headers.insert(name.to_owned(), None);
    }
    let headers = capture_anthropic_headers(model, context, options, Vec::new());
    for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
        assert!(
            !headers.contains_key(name),
            "model deletion restored {name}"
        );
    }
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:880-970` merges explicit request
/// headers after defaults and model headers, before the final transform.
#[test]
fn headers_explicit_before_transform() {
    let (_, mut model, context, mut options) = parse_fixture("text-only");
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.send_session_affinity_headers = Some(true);
    options.session_id = Some("request-delete-session".to_owned());
    for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
        options.headers.insert(name.to_owned(), None);
    }
    let headers = capture_anthropic_headers(
        model,
        context,
        options,
        vec![Arc::new(AssertAnthropicDefaultsAbsent)],
    );
    for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
        assert!(
            !headers.contains_key(name),
            "request deletion restored {name}"
        );
    }
}

/// Architecture v2 part 2 §2.6 and §10.4; the final Models-level transform
/// may delete every Anthropic default and no API or transport layer re-adds it.
#[test]
fn headers_transform_can_delete_default() {
    let (_, mut model, context, mut options) = parse_fixture("text-only");
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.send_session_affinity_headers = Some(true);
    options.session_id = Some("transform-delete-session".to_owned());
    let headers = capture_anthropic_headers(
        model,
        context,
        options,
        vec![Arc::new(DeleteAnthropicDefaults)],
    );
    for name in ANTHROPIC_DEFAULT_HEADER_NAMES {
        assert!(!headers.contains_key(name), "transport restored {name}");
    }
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:288-306,536-544` validates
/// header-owned auth after Models-level header transformation. A deleted
/// `ANTHROPIC_AUTH_TOKEN` bearer header must fail before Send transport.
#[test]
fn anthropic_final_headers_require_auth_send() {
    let transport = Arc::new(CountingTransport::default());
    let registration =
        anthropic_provider(Arc::clone(&transport) as Arc<dyn HttpTransport>).expect("registration");
    let model = registration
        .catalog
        .snapshot()
        .first()
        .expect("Anthropic model")
        .clone();
    let auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("ANTHROPIC_AUTH_TOKEN".to_owned(), "header-token".to_owned())]),
        Vec::<String>::new(),
    ));
    let models = Models::builder()
        .auth_context(auth_context)
        .provider(registration)
        .header_transform(Arc::new(DeleteAnthropicAuthHeaders))
        .build()
        .expect("Anthropic Models");
    let (_, _, context, _) = parse_fixture("text-only");

    let error = match futures_executor::block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context,
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    )) {
        Ok(_) => panic!("deleted header-owned auth must reject before Send transport"),
        Err(error) => error,
    };

    assert_eq!(error.kind, RequestStartErrorKind::RuntimeUnavailable);
    assert_eq!(error.message, "No API key for provider: anthropic");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

/// Architecture v2 part 2 §2.6, §9.2, and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:288-306,536-544` applies the
/// same final header-owned auth assertion on the Local execution family.
#[test]
fn anthropic_final_headers_require_auth_local() {
    let transport = Rc::new(LocalCountingTransport::default());
    let registration =
        local_anthropic_provider(Rc::clone(&transport) as Rc<dyn LocalHttpTransport>)
            .expect("local registration");
    let model = registration
        .catalog
        .snapshot()
        .first()
        .expect("local Anthropic model")
        .clone();
    let auth_context = Rc::new(MapAuthContext::new(
        BTreeMap::from([("ANTHROPIC_AUTH_TOKEN".to_owned(), "header-token".to_owned())]),
        Vec::<String>::new(),
    ));
    let models = LocalModels::builder()
        .auth_context(auth_context)
        .provider(registration)
        .header_transform(Rc::new(LocalDeleteAnthropicAuthHeaders))
        .build()
        .expect("local Anthropic Models");
    let (_, _, context, _) = parse_fixture("text-only");

    let error = match futures_executor::block_on(LocalModelRuntime::stream(
        &models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context,
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    )) {
        Ok(_) => panic!("deleted header-owned auth must reject before Local transport"),
        Err(error) => error,
    };

    assert_eq!(error.kind, RequestStartErrorKind::RuntimeUnavailable);
    assert_eq!(error.message, "No API key for provider: anthropic");
    assert_eq!(transport.calls.get(), 0);
}

/// Architecture v2 part 2 §2.5 and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:562-568` applies `onPayload`
/// before spreading the transformed params and reasserting `stream: true`.
#[test]
fn anthropic_payload_transform_cannot_disable_stream_send() {
    let (_, model, context, options) = parse_fixture("text-only");
    let transport = Arc::new(FixturePipelineTransport::new([read_bytes(
        "text-only",
        "response-turn-1.sse",
    )]));
    let mut headers = anthropic_default_headers();
    headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("Send stream-override registration");
    let models = Models::builder()
        .provider(registration)
        .erased_payload_transform(Arc::new(ReplaceAnthropicPayloadWithStreamingDisabled))
        .build()
        .expect("Send stream-override Models");

    let _ = run_fixture_pipeline(&models, &model, context, options);
    let requests = transport.requests.lock().expect("Send stream requests");
    assert_eq!(requests[0].body, br#"{"replacement":true,"stream":true}"#);
}

/// Architecture v2 part 2 §2.5, §9.2, and §10.4; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:562-568` imposes the same final
/// streaming invariant on the Local execution family.
#[test]
fn anthropic_payload_transform_cannot_disable_stream_local() {
    let (_, model, context, options) = parse_fixture("text-only");
    let transport = Rc::new(LocalFixturePipelineTransport::new([read_bytes(
        "text-only",
        "response-turn-1.sse",
    )]));
    let mut headers = anthropic_default_headers();
    headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = LocalProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            local_anthropic_messages_api(Rc::clone(&transport) as Rc<dyn LocalHttpTransport>),
        )
        .build()
        .expect("Local stream-override registration");
    let models = LocalModels::builder()
        .provider(registration)
        .erased_payload_transform(Rc::new(ReplaceAnthropicPayloadWithStreamingDisabled))
        .build()
        .expect("Local stream-override Models");

    let _ = run_local_fixture_pipeline(&models, &model, context, options);
    let requests = transport.requests.borrow();
    assert_eq!(requests[0].body, br#"{"replacement":true,"stream":true}"#);
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:544-568,880-1110` and the
/// captured Anthropic Messages request corpus.
#[test]
fn wire_anthropic_messages_pi_exact() {
    for case in case_names() {
        let (fixture, model, context, simple) = parse_fixture(&case);
        let actual = encode_fixture(
            fixture["entrypoint"].as_str().expect("entrypoint"),
            &model,
            &context,
            &simple,
        );
        assert_eq!(
            String::from_utf8(actual).expect("actual UTF-8"),
            String::from_utf8(read_bytes(&case, "request-turn-1.body.json"))
                .expect("expected UTF-8"),
            "turn one wire mismatch for {case}"
        );
        assert_eq!(
            String::from_utf8(encode_turn_two(&case)).expect("actual UTF-8"),
            String::from_utf8(read_bytes(&case, "request-turn-2.body.json"))
                .expect("expected UTF-8"),
            "turn two wire mismatch for {case}"
        );
        assert_fixture_pipeline(&case, &fixture, &model, context, &simple);
    }
}

/// Architecture v2 part 2 §3.5 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1342-1360` uses nullish
/// coalescing for legacy non-strict `properties` and `required` members.
#[test]
fn anthropic_non_strict_null_schema_normalizes_pi_exact() {
    let (_, model, mut context, mut options) = parse_fixture("text-only");
    context.tools = vec![ToolSpec {
        schema_version: 1,
        name: "null_schema".to_owned(),
        description: "Null schema members".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": null,
            "required": null
        }),
        constrained_sampling: None,
    }];
    options.cache_retention = Some(CacheRetention::None);

    let actual = encode_fixture("stream", &model, &context, &options);
    let expected = br#"{"model":"fixture-anthropic-model","messages":[{"role":"user","content":"Return a concise fixture response."}],"max_tokens":8192,"stream":true,"tools":[{"name":"null_schema","description":"Null schema members","eager_input_streaming":true,"input_schema":{"type":"object","properties":{},"required":[]}}]}"#;
    assert_eq!(actual, expected);
}

/// Architecture v2 part 2 §3.5 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1168-1207` preserves array-form
/// user content as individual blocks, including when every block is text.
#[test]
fn anthropic_user_text_block_array_preserves_blocks_pi_exact() {
    let (_, model, mut context, _) = parse_fixture("text-only");
    context.messages = vec![Message::User(pi_ai::UserMessage {
        id: MessageId::new("multi-text-user"),
        content: vec![
            ContentBlock::Text {
                id: ContentBlockId::new("multi-text-0"),
                text: "first".to_owned(),
            },
            ContentBlock::Text {
                id: ContentBlockId::new("multi-text-1"),
                text: "second".to_owned(),
            },
        ],
        timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
    })];

    let encoded = encode_context(&model, &context);
    assert_eq!(
        encoded["messages"][0]["content"],
        serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "text", "text": "second" }
        ])
    );
}

/// Architecture v2 part 2 §5.1, §6.1, and §10.7; pinned Pi basis:
/// `packages/ai/src/providers/anthropic.ts` and the pinned generated Anthropic
/// catalog register one Anthropic Messages API plus API-key and OAuth auth.
#[test]
fn anthropic_provider_registration_matches_pinned_catalog() {
    // Pi basis: packages/ai/src/providers/anthropic.ts and the pinned generated
    // providers/data/anthropic.json catalog.
    let catalog = anthropic_models().expect("Anthropic catalog");
    assert_eq!(catalog.len(), 13);
    assert!(catalog.iter().all(|model| {
        model.common.model_ref.provider.as_str() == "anthropic"
            && matches!(model.api, ApiModelConfig::AnthropicMessages(_))
    }));
    let registration = anthropic_provider(Arc::new(NeverTransport)).expect("registration");
    assert_eq!(registration.catalog.snapshot().len(), 13);
    assert!(
        registration
            .apis
            .contains_key(&pi_ai::ApiId::new("anthropic-messages"))
    );
}

/// Architecture v2 part 2 §6.1 and §10.7; pinned Pi basis:
/// `packages/ai/src/providers/anthropic.ts:10-16` names the API-key method
/// `Anthropic API key`, which produces the prompt `Enter Anthropic API key`.
#[test]
fn anthropic_api_key_login_label_and_prompt_match_pi() {
    let registration = anthropic_provider(Arc::new(NeverTransport)).expect("registration");
    let interaction = Arc::new(RecordingApiKeyInteraction::new([
        AuthAnswer::Selected("api_key".to_owned()),
        AuthAnswer::Text("entered-key".to_owned()),
    ]));

    let credential = futures_executor::block_on(registration.auth.login(
        Arc::clone(&interaction) as Arc<dyn AuthInteraction>,
        CancellationToken::new(),
    ))
    .expect("API-key login");
    let Credential::ApiKey(credential) = credential else {
        panic!("API-key selection must return an API-key credential")
    };
    assert_eq!(
        credential.key.as_ref().map(SecretString::expose_secret),
        Some("entered-key")
    );

    let prompts = interaction.prompts.lock().expect("prompt lock");
    let AuthPrompt::Select { options, .. } = &prompts[0] else {
        panic!("first prompt must select the authentication method")
    };
    assert_eq!(options[1].id, "api_key");
    assert_eq!(options[1].label, "Anthropic API key");
    assert_eq!(
        prompts[1],
        AuthPrompt::Secret {
            message: "Enter Anthropic API key".to_owned(),
            placeholder: None,
        }
    );
}

/// Architecture v2 part 2 §6.1 and §10.7; pinned Pi basis:
/// `packages/ai/src/providers/anthropic.ts:10-45` and
/// `packages/ai/test/anthropic-auth-token.test.ts:65-214` define environment
/// precedence and Bearer-versus-key shaping.
#[test]
fn anthropic_auth_token_precedence_matches_pi() {
    // Pi basis: packages/ai/src/providers/anthropic.ts and
    // packages/ai/test/anthropic-auth-token.test.ts.
    let registration = anthropic_provider(Arc::new(NeverTransport)).expect("registration");
    let mut environment = std::collections::BTreeMap::new();
    environment.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "auth-token".to_owned());
    environment.insert("ANTHROPIC_OAUTH_TOKEN".to_owned(), "oauth-token".to_owned());
    environment.insert("ANTHROPIC_API_KEY".to_owned(), "api-key".to_owned());
    let mut request = pi_ai::ResolveAuthRequest::isolated(registration.descriptor.clone(), None);
    request.auth_context = Arc::new(MapAuthContext::new(environment, []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect("auth resolution")
            .expect("resolved auth");
    assert_eq!(resolved.source.0, "ANTHROPIC_AUTH_TOKEN");
    assert_eq!(
        resolved
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer auth-token")
    );
    assert!(!resolved.headers.contains_key("x-api-key"));
    assert!(resolved.api_key.is_none());

    let mut environment = std::collections::BTreeMap::new();
    environment.insert("ANTHROPIC_OAUTH_TOKEN".to_owned(), "oauth-token".to_owned());
    environment.insert("ANTHROPIC_API_KEY".to_owned(), "api-key".to_owned());
    let mut request = pi_ai::ResolveAuthRequest::isolated(registration.descriptor.clone(), None);
    request.auth_context = Arc::new(MapAuthContext::new(environment, []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect("OAuth-token environment resolution")
            .expect("OAuth-token environment value");
    assert_eq!(resolved.source.0, "ANTHROPIC_OAUTH_TOKEN");
    assert_eq!(
        resolved.api_key.as_ref().map(SecretString::expose_secret),
        Some("oauth-token")
    );
    assert_eq!(
        resolved
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("oauth-token")
    );
    assert!(!resolved.headers.contains_key(http::header::AUTHORIZATION));
    assert!(!resolved.headers.contains_key("x-app"));

    let mut environment = std::collections::BTreeMap::new();
    environment.insert(
        "ANTHROPIC_OAUTH_TOKEN".to_owned(),
        "sk-ant-oat-environment".to_owned(),
    );
    let mut request = pi_ai::ResolveAuthRequest::isolated(registration.descriptor.clone(), None);
    request.auth_context = Arc::new(MapAuthContext::new(environment, []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect("Claude Code environment resolution")
            .expect("Claude Code environment value");
    assert_eq!(resolved.source.0, "ANTHROPIC_OAUTH_TOKEN");
    assert_eq!(
        resolved.api_key.as_ref().map(SecretString::expose_secret),
        Some("sk-ant-oat-environment")
    );
    assert_eq!(
        resolved
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer sk-ant-oat-environment")
    );
    assert_eq!(
        resolved
            .headers
            .get("x-app")
            .and_then(|value| value.to_str().ok()),
        Some("cli")
    );
}

/// Architecture v2 part 2 §6.1-§6.6 and §10.7; pinned Pi basis:
/// `packages/ai/src/providers/anthropic.ts:48-57` registers Anthropic OAuth
/// alongside API-key auth, and `anthropic-messages.ts:918-944` applies the
/// Claude Code identity headers to an OAuth credential.
#[test]
fn anthropic_stored_oauth_credential_resolves() {
    let registration = anthropic_provider(Arc::new(NeverTransport)).expect("registration");
    let store = Arc::new(InMemoryCredentialStore::new());
    futures_executor::block_on(store.modify(
        ProviderId::new("anthropic"),
        CancellationToken::new(),
        |_| async {
            Ok(Some(Credential::OAuth(OAuthCredential {
                access: SecretString::new("sk-ant-oat-stored"),
                refresh: SecretString::new("refresh-token"),
                expires_at: Timestamp::from_unix_millis(9_007_199_254_740_991),
                extra: ProviderOAuthExtra::None,
            })))
        },
    ))
    .expect("store Anthropic OAuth credential");
    let mut request = ResolveAuthRequest::isolated(registration.descriptor.clone(), None);
    request.credential_store = store;
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect("resolve Anthropic OAuth")
            .expect("stored OAuth is supported");
    assert_eq!(resolved.source.0, "OAuth");
    assert_eq!(
        resolved
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer sk-ant-oat-stored")
    );
    assert_eq!(
        resolved
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("claude-code-20250219,oauth-2025-04-20")
    );
    assert_eq!(
        resolved
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some("claude-cli/2.1.75")
    );
    assert_eq!(resolved.headers.get("x-app").expect("OAuth x-app"), "cli");
}

/// Architecture v2 part 2 §6.4 and §10.7; pinned Pi basis:
/// `packages/ai/src/auth/oauth/anthropic.ts:268-320` and
/// `packages/ai/test/anthropic-oauth.test.ts:76-105` omit `scope` from the
/// refresh-token request while retaining the fixed client identifier.
#[test]
fn anthropic_oauth_refresh_omits_scope() {
    let transport = Arc::new(FixturePipelineTransport::new([
        br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
            .to_vec(),
    ]));
    let oauth = AnthropicOAuth::new(Arc::clone(&transport) as Arc<dyn HttpTransport>);
    let credential = futures_executor::block_on(OAuthAuth::refresh(
        &oauth,
        OAuthCredential {
            access: SecretString::new("old-access"),
            refresh: SecretString::new("refresh-token"),
            expires_at: Timestamp::from_unix_millis(0),
            extra: ProviderOAuthExtra::None,
        },
        CancellationToken::new(),
    ))
    .expect("Anthropic OAuth refresh");
    assert_eq!(credential.access.expose_secret(), "new-access");
    assert_eq!(credential.refresh.expose_secret(), "new-refresh");
    let requests = transport
        .requests
        .lock()
        .expect("OAuth refresh request lock");
    assert_eq!(
        requests[0].url.as_str(),
        "https://platform.claude.com/v1/oauth/token"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).expect("refresh body JSON");
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "refresh-token");
    assert!(
        body["client_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(body.get("scope").is_none());
}

/// Architecture v2 part 2 §3.5, §6.6, and §10.7; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:81-113,637-651,973-1034` and
/// `packages/ai/test/anthropic-tool-name-normalization.test.ts` apply Claude
/// Code casing outbound and restore the caller's exact tool name inbound.
#[test]
fn anthropic_oauth_tool_names_and_identity_round_trip() {
    let response = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_oauth_tool\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_oauth\",\"name\":\"TodoWrite\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"task\\\":\\\"buy milk\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    )
    .as_bytes()
    .to_vec();
    let transport = Arc::new(FixturePipelineTransport::new([response]));
    let registration = anthropic_provider(Arc::clone(&transport) as Arc<dyn HttpTransport>)
        .expect("Anthropic OAuth registration");
    let model = registration
        .catalog
        .snapshot()
        .iter()
        .find(|model| model.common.model_ref.model.as_str() == "claude-sonnet-4-6")
        .expect("Claude Sonnet 4.6")
        .clone();
    let store = Arc::new(InMemoryCredentialStore::new());
    futures_executor::block_on(store.modify(
        ProviderId::new("anthropic"),
        CancellationToken::new(),
        |_| async {
            Ok(Some(Credential::OAuth(OAuthCredential {
                access: SecretString::new("sk-ant-oat-tool-test"),
                refresh: SecretString::new("refresh-token"),
                expires_at: Timestamp::from_unix_millis(9_007_199_254_740_991),
                extra: ProviderOAuthExtra::None,
            })))
        },
    ))
    .expect("store OAuth credential");
    let models = Models::builder()
        .credential_store(store)
        .provider(registration)
        .build()
        .expect("OAuth Models");
    let context = Context {
        schema_version: 1,
        system_prompt: Some("Caller system prompt.".to_owned()),
        messages: vec![Message::User(pi_ai::UserMessage {
            id: MessageId::new("oauth-user"),
            content: vec![ContentBlock::Text {
                id: ContentBlockId::new("oauth-user-text"),
                text: "Add a todo.".to_owned(),
            }],
            timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        })],
        tools: vec![ToolSpec {
            schema_version: 1,
            name: "todowrite".to_owned(),
            description: "Write one todo".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"task": {"type": "string"}},
                "required": ["task"]
            }),
            constrained_sampling: None,
        }],
    };
    let events = run_fixture_pipeline(&models, &model, context, SimpleGenerationOptions::default());
    let message = terminal_message(&events);
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall { call, .. }
            if call.name == "todowrite" && call.arguments == serde_json::json!({"task":"buy milk"})
    ));
    let requests = transport.requests.lock().expect("OAuth request lock");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("OAuth request JSON");
    assert_eq!(body["tools"][0]["name"], "TodoWrite");
    assert_eq!(
        body["system"],
        serde_json::json!([
            {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral"}},
            {"type":"text","text":"Caller system prompt.","cache_control":{"type":"ephemeral"}}
        ])
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("claude-code-20250219,oauth-2025-04-20")
    );
}

/// Architecture v2 part 2 §10.2 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:623-705,1195-1245` preserves a
/// completed visible-thinking signature in the exact-model second turn.
#[test]
fn anthropic_signed_thinking_turn_two_pi_exact() {
    assert_eq!(
        encode_turn_two("signed-thinking-replay"),
        read_bytes("signed-thinking-replay", "request-turn-2.body.json")
    );
}

/// Architecture v2 part 2 §10.2 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:637-651,1210-1220` preserves the
/// exact redacted-thinking data payload in the second turn.
#[test]
fn anthropic_redacted_thinking_turn_two_pi_exact() {
    assert_eq!(
        encode_turn_two("redacted-encrypted-reasoning-replay"),
        read_bytes(
            "redacted-encrypted-reasoning-replay",
            "request-turn-2.body.json"
        )
    );
}

/// Architecture v2 part 2 §1.4 and §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:685-705` appends every
/// `signature_delta` to the active thinking signature in provider order.
#[test]
fn anthropic_signature_fragments_append_in_order() {
    let (_, model, _, _) = parse_fixture("signed-thinking-replay");
    let output = decode_turn_one("signed-thinking-replay", &model);
    let signature = output
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == ANTHROPIC_THINKING_SIGNATURE_KIND)
        .and_then(ReplayItem::as_utf8);
    assert_eq!(signature, Some("signed-fixture-reasoning"));
}

/// Architecture v2 part 2 §1.2 and §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:623-705` requires the completed
/// signature to remain available after durable message persistence.
#[test]
fn anthropic_signature_survives_message_round_trip() {
    let (_, model, _, _) = parse_fixture("signed-thinking-replay");
    let output = decode_turn_one("signed-thinking-replay", &model);
    let restored: AssistantMessage =
        serde_json::from_slice(&serde_json::to_vec(&output).expect("serialize assistant"))
            .expect("deserialize assistant");
    assert_eq!(restored.replay, output.replay);
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1195-1245` re-encodes a complete
/// exact-scope signature on the next request.
#[test]
fn anthropic_turn_two_replays_exact_signature() {
    anthropic_signed_thinking_turn_two_pi_exact();
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1210-1220` re-encodes redacted
/// thinking as its opaque `data` block.
#[test]
fn anthropic_redacted_thinking_replays_exact_data() {
    anthropic_redacted_thinking_turn_two_pi_exact();
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1221-1243` converts unsigned
/// visible thinking to ordinary text unless empty signatures are allowed.
#[test]
fn anthropic_unsigned_thinking_falls_back_to_text() {
    let (mut model, mut context, output) = empty_signature_message();
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.allow_empty_signature = Some(false);
    context.messages.push(Message::Assistant(output));
    append_follow_up(&mut context);
    let body = encode_context(&model, &context);
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
}

/// Architecture v2 part 2 §1.4 and §10.2; pinned Pi basis:
/// `packages/ai/test/anthropic-empty-thinking-signature-compat.test.ts` and
/// `anthropic-messages.ts:1221-1243` preserve compat-allowed empty signatures.
#[test]
fn anthropic_empty_signature_respects_compat() {
    let (mut model, mut context, output) = empty_signature_message();
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.allow_empty_signature = Some(true);
    context.messages.push(Message::Assistant(output));
    append_follow_up(&mut context);
    let body = encode_context(&model, &context);
    assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(body["messages"][1]["content"][0]["signature"], "");
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:685-715` retains partial stream
/// state on failure, while replay invariant R3 forbids replaying it as complete.
#[test]
fn anthropic_failed_partial_signature_is_not_replayed() {
    let (_, model, _, _) = parse_fixture("signed-thinking-replay");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_partial\",\"model\":\"fixture-anthropic-model\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"partial\",\"signature\":\"sig-\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"fragment\"}}\n\n"
    );
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "partial"));
    let failed = events
        .into_iter()
        .find_map(|event| match event {
            AssistantEvent::Failed { message } => Some(message),
            _ => None,
        })
        .expect("failed terminal message");
    assert!(failed.replay.items.iter().all(|item| {
        item.kind.as_str() != ANTHROPIC_THINKING_SIGNATURE_KIND
            || item.completeness == ReplayCompleteness::Incomplete
    }));
}

/// Architecture v2 part 2 §1.2, §2.2, and §10.2; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1195-1245` only sends Anthropic
/// signatures back to the same provider/API/model scope.
#[test]
fn anthropic_signature_never_crosses_model_boundary() {
    let (_, source_model, mut context, _) = parse_fixture("signed-thinking-replay");
    context.messages.push(Message::Assistant(decode_turn_one(
        "signed-thinking-replay",
        &source_model,
    )));
    append_follow_up(&mut context);
    let mut target_model = source_model;
    target_model.common.model_ref.model = ModelId::new("different-anthropic-model");
    let body = encode_context(&target_model, &context);
    assert_eq!(body["messages"][1]["content"][0]["type"], "text");
    assert!(body["messages"][1]["content"][0].get("signature").is_none());
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:840-878,1063-1086` uses adaptive
/// thinking plus a native effort when `forceAdaptiveThinking` is enabled.
#[test]
fn anthropic_adaptive_uses_effort() {
    let body = encode_case("reasoning-minimal");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "minimal");
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:852-878,1075-1086` uses a bounded
/// token budget and retains 1024 tokens of answer room on legacy models.
#[test]
fn anthropic_budget_model_uses_budget_tokens() {
    let body = encode_case("signed-thinking-replay");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 2048);
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1035-1039` omits temperature
/// whenever extended thinking is enabled.
#[test]
fn anthropic_temperature_omitted_while_thinking() {
    let body = encode_case("reasoning-low");
    assert!(body.get("temperature").is_none());
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/test/anthropic-temperature-compat.test.ts` and
/// `anthropic-messages.ts:1035-1039` omit unsupported temperature values.
#[test]
fn anthropic_temperature_omitted_when_model_disallows_it() {
    let (_, mut model, context, mut simple) = parse_fixture("text-only");
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_temperature = Some(false);
    simple.temperature = Some(0.75);
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("wire JSON");
    assert!(body.get("temperature").is_none());
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/test/anthropic-thinking-disable.test.ts` and
/// `anthropic-messages.ts:1087-1089` emit disabled thinking except for an
/// explicitly unsupported catalog `off` mapping.
#[test]
fn anthropic_disabled_thinking_respects_compat() {
    let (fixture, mut model, context, mut simple) = parse_fixture("thinking-disabled");
    let body: Value = serde_json::from_slice(&encode_fixture(
        fixture["entrypoint"].as_str().expect("entrypoint"),
        &model,
        &context,
        &simple,
    ))
    .expect("wire JSON");
    assert_eq!(body["thinking"]["type"], "disabled");

    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.thinking_levels.off = Some(LevelSupport::Unsupported);
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("wire JSON");
    assert!(body.get("thinking").is_none());

    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.thinking_levels.off = None;
    simple.reasoning = Some(ReasoningLevel::Off);
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("explicit off with default nonadaptive map");
    assert_eq!(body["thinking"]["type"], "disabled");

    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.force_adaptive_thinking = Some(true);
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("explicit off with adaptive compatibility");
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("output_config").is_none());
}

/// Architecture v2 part 2 §2.6 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:819-866,969-976` calls
/// `buildBaseOptions` on the original context before `buildParams` calls
/// `transformMessages`. A failed assistant is omitted from the wire history,
/// but its durable content still constrains the simple-call output cap.
#[test]
fn anthropic_lowering_precedes_projection_for_output_cap() {
    let (_, mut model, mut context, mut simple) = parse_fixture("text-only");
    context.messages.push(Message::Assistant(parse_assistant(
        &serde_json::json!({
            "role": "assistant",
            "provider": model.common.model_ref.provider.as_str(),
            "api": "anthropic-messages",
            "model": model.common.model_ref.model.as_str(),
            "content": [{"type": "text", "text": "x".repeat(4_000)}],
            "stopReason": "error",
            "errorMessage": "fixture failure",
            "timestamp": FIXTURE_TIMESTAMP
        }),
        1,
        &model,
    )));
    simple.max_output_tokens = Some(1_000);
    simple.cache_retention = Some(CacheRetention::None);

    let original_estimate = estimate_context_tokens(&context).expect("estimate original context");
    let projected = transform_context_for_model(
        &context,
        &model,
        &Default::default(),
        &AnthropicMessagesHandoff,
    )
    .expect("project context")
    .context;
    let projected_estimate =
        estimate_context_tokens(&projected).expect("estimate projected context");
    assert!(original_estimate.tokens > projected_estimate.tokens);

    const EXPECTED_CAP: u64 = 37;
    model.common.limits.context_window = original_estimate
        .tokens
        .saturating_add(pi_ai::CONTEXT_SAFETY_TOKENS)
        .saturating_add(EXPECTED_CAP);
    let response = read_bytes("text-only", "response-turn-1.sse");
    let transport = Arc::new(FixturePipelineTransport::new([response]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("Anthropic cap-regression registration");
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("Anthropic cap-regression Models");

    let events = run_fixture_pipeline(&models, &model, context, simple);
    assert_eq!(
        terminal_message(&events).finish.reason,
        AssistantFinishReason::Stop
    );
    let requests = transport.requests.lock().expect("request lock");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("wire JSON");
    assert_eq!(body["max_tokens"], EXPECTED_CAP);
    assert_eq!(
        body["messages"].as_array().expect("wire messages").len(),
        1,
        "failed assistant must remain omitted from the projected wire context"
    );
}

/// Architecture v2 part 2 §3.4 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/simple-options.ts:11-18` bypasses context estimation
/// and retains the requested/default output plan when `contextWindow <= 0`.
#[test]
fn anthropic_zero_context_window_preserves_output_cap() {
    let (_, mut model, context, mut simple) = parse_fixture("text-only");
    model.common.limits.context_window = 0;
    model.common.limits.max_output_tokens = 4_096;
    model.common.reasoning = true;
    simple.max_output_tokens = Some(777);
    simple.cache_retention = Some(CacheRetention::None);

    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("zero-window non-reasoning wire JSON");
    assert_eq!(body["max_tokens"], 777);

    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.thinking_levels.low = Some(LevelSupport::Value(pi_ai::AnthropicThinkingValue::Budget(
        2_048,
    )));
    simple.reasoning = Some(ReasoningLevel::Low);
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("zero-window mapped-budget wire JSON");
    assert_eq!(body["max_tokens"], 2_825);

    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.thinking_levels.low = None;
    let body: Value =
        serde_json::from_slice(&encode_fixture("streamSimple", &model, &context, &simple))
            .expect("zero-window default-budget wire JSON");
    assert_eq!(body["max_tokens"], 2_825);
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `packages/ai/test/anthropic-sse-parsing.test.ts:82-168` and
/// `packages/ai/src/utils/json-parse.ts:31-94` repair both the outer SSE JSON
/// string and the streamed tool-argument JSON.
#[test]
fn anthropic_sse_malformed_json_is_repaired() {
    let (_, model, _, _) = parse_fixture("text-only");
    let malformed = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;
    let body = [
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_repair\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_test\",\"name\":\"edit\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: "
        ),
        malformed,
        concat!(
            "\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        ),
    ]
    .concat();
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "repair"));
    let message = terminal_message(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::ToolUse);
    let arguments = message.content.iter().find_map(|block| match block {
        ContentBlock::ToolCall { call, .. } => Some(&call.arguments),
        _ => None,
    });
    assert_eq!(
        arguments,
        Some(&serde_json::json!({"path": "A\\H", "text": "col1\tcol2"}))
    );
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:382-433` decodes through the
/// platform `TextDecoder`, which strips one initial UTF-8 BOM even when its
/// bytes arrive in separate chunks.
#[test]
fn anthropic_initial_utf8_bom_split_across_chunks_matches_pi() {
    let (_, model, _, _) = parse_fixture("text-only");
    let mut decoder = AnthropicMessagesSseDecoder::new(decode_context_for(&model, "split-bom"));
    let mut events = decoder.take_events();
    events.extend(decoder.push(&[0xef]));
    events.extend(decoder.push(&[0xbb]));
    let response = read_bytes("text-only", "response-turn-1.sse");
    let mut final_chunk = Vec::with_capacity(response.len() + 1);
    final_chunk.push(0xbf);
    final_chunk.extend(response);
    events.extend(decoder.push(&final_chunk));
    events.extend(decoder.finish());

    let message = terminal_message(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "fixture response turn 1"
    ));
}

/// Architecture v2 part 2 §1.4 and §10.1; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:315-433` frames SSE one line at
/// a time, so an empty line may use a different valid ending than the prior
/// data line. The first mixed delimiter is also split after its CR byte.
#[test]
fn anthropic_sse_mixed_line_endings_split_across_chunks_match_pi() {
    let (_, model, _, _) = parse_fixture("text-only");
    let source = String::from_utf8(read_bytes("text-only", "response-turn-1.sse"))
        .expect("fixture SSE UTF-8");
    let mut mixed = String::new();
    for (index, record) in source.trim_end_matches('\n').split("\n\n").enumerate() {
        mixed.push_str(record);
        mixed.push_str(match index % 3 {
            0 => "\r\n\n",
            1 => "\n\r\n",
            _ => "\r\r\n",
        });
    }
    let split = mixed.find("\r\n\n").expect("mixed delimiter") + 1;

    let mut decoder =
        AnthropicMessagesSseDecoder::new(decode_context_for(&model, "mixed-split-endings"));
    let mut events = decoder.take_events();
    events.extend(decoder.push(&mixed.as_bytes()[..split]));
    events.extend(decoder.push(&mixed.as_bytes()[split..]));
    events.extend(decoder.finish());

    let message = terminal_message(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "fixture response turn 1"
    ));
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:590-760` fails when recognized
/// event variants omit objects or scalar fields that the implementation reads.
#[test]
fn anthropic_recognized_malformed_events_fail_stream() {
    let (_, model, _, _) = parse_fixture("text-only");
    let valid_start = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_malformed\",\"usage\":{}}}\n\n"
    );
    let valid_text_start = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
    );
    for (suffix, body, expected_field) in [
        (
            "missing-message",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\"}\n\n"
            )
            .to_owned(),
            "`message`",
        ),
        (
            "missing-usage",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\"}}\n\n"
            )
            .to_owned(),
            "`usage`",
        ),
        (
            "missing-start-index",
            format!(
                "{valid_start}event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"text\",\"text\":\"hello\"}}}}\n\n"
            ),
            "`index`",
        ),
        (
            "missing-delta-text",
            format!(
                "{valid_start}{valid_text_start}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\"}}}}\n\n"
            ),
            "`text`",
        ),
        (
            "missing-stop-index",
            format!(
                "{valid_start}{valid_text_start}event: content_block_stop\ndata: {{\"type\":\"content_block_stop\"}}\n\n"
            ),
            "`index`",
        ),
        (
            "missing-message-delta",
            format!("{valid_start}event: message_delta\ndata: {{\"type\":\"message_delta\"}}\n\n"),
            "`delta`",
        ),
    ] {
        let events =
            decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, suffix));
        let failed = events.iter().find_map(|event| match event {
            AssistantEvent::Failed { message } => Some(message),
            _ => None,
        });
        let failed = failed.unwrap_or_else(|| panic!("{suffix} must fail"));
        assert_eq!(failed.finish.reason, AssistantFinishReason::Error);
        assert!(
            failed
                .finish
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains(expected_field)),
            "{suffix} did not identify {expected_field}: {:?}",
            failed.finish.error
        );
    }
}

/// Architecture v2 part 2 §1.2, §1.4, §10.1, and replay invariant R7;
/// pinned Pi basis: `anthropic-sse-parsing.test.ts:169-266` preserves start
/// content, while `anthropic-messages.ts:623-670` retains provider indexes.
#[test]
fn anthropic_content_start_and_replay_provider_index_are_preserved() {
    let (_, model, _, _) = parse_fixture("text-only");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_initial\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"Initial text\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\" plus delta\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"Initial thinking\",\"signature\":\"initial signature\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" plus delta\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"signature_delta\",\"signature\":\" plus delta\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let events = decode_anthropic_messages_sse(
        body.as_bytes(),
        decode_context_for(&model, "initial-content"),
    );
    let message = terminal_message(&events);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "Initial text plus delta"
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::Thinking { text, .. } if text == "Initial thinking plus delta"
    ));
    assert_eq!(message.replay.items[0].ordinal, 4);
    assert_eq!(
        message.replay.items[0].as_utf8(),
        Some("initial signature plus delta")
    );
}

/// Architecture v2 part 2 §1.3 and §10.1; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:611-652` ignores unknown
/// provider content-block types without inserting canonical content.
#[test]
fn anthropic_unknown_content_block_does_not_consume_content_index() {
    let (_, model, _, _) = parse_fixture("text-only");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_unknown_block\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"future_block\",\"value\":\"ignored\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"Known text\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "unknown-block"));

    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ContentBlockStarted {
            content_index: 0,
            kind: pi_ai::ContentBlockKind::Text,
            ..
        }
    )));
    let message = terminal_message(&events);
    assert_eq!(message.content.len(), 1);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "Known text"
    ));
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `packages/ai/test/anthropic-sse-parsing.test.ts:376-403` treats a
/// `message_delta` without usage as a no-op for cumulative usage.
#[test]
fn anthropic_message_delta_without_usage_preserves_prior_usage() {
    let (_, model, _, _) = parse_fixture("text-only");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_no_usage\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "no-usage"));
    let message = terminal_message(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert_eq!(message.usage.input_tokens, 12);
    assert_eq!(message.usage.output_tokens, 0);
}

/// Architecture v2 part 2 §1.3 and §10.1; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:590-599` assigns every
/// provider-reported `message.model`, including the requested model itself.
#[test]
fn stream_response_model_is_preserved() {
    let (_, model, _, _) = parse_fixture("text-only");
    let reported = model.common.model_ref.model.as_str();
    let body = format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_same_model\",\"model\":\"{}\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        reported
    );
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "same-model"));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ResponseMetadata {
            response_model: Some(response_model),
            ..
        } if response_model.as_str() == reported
    )));
    assert_eq!(
        terminal_message(&events)
            .response_model
            .as_ref()
            .map(ModelId::as_str),
        Some(reported)
    );
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `packages/ai/test/anthropic-sse-parsing.test.ts:405-423` ignores unknown
/// proxy SSE event types, including events following `message_stop`.
#[test]
fn anthropic_unknown_sse_events_after_message_stop_are_ignored() {
    let (_, model, _, _) = parse_fixture("text-only");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_unknown_tail\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        "event: done\n",
        "data: [DONE]\n\n",
        "event: proxy.stats\n",
        "data: not json\n\n"
    );
    let events =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "unknown-tail"));
    let message = terminal_message(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "Hello"
    ));
}

/// Architecture v2 part 2 §2 and §10.1; pinned Pi basis:
/// `packages/ai/test/anthropic-sse-parsing.test.ts:267-375` retains Anthropic
/// refusal details and the provider's raw stop reason on failed assistants.
#[test]
fn anthropic_refusal_and_sensitive_stop_details_match_pi() {
    let (_, model, _, _) = parse_fixture("text-only");
    for (suffix, reason, details, expected) in [
        (
            "refusal",
            "refusal",
            ",\"stop_details\":{\"type\":\"refusal\",\"explanation\":\"fixture refusal\"}",
            "fixture refusal",
        ),
        (
            "sensitive",
            "sensitive",
            "",
            "Provider stopped with: sensitive",
        ),
    ] {
        let body = format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_stop\",\"usage\":{{}}}}}}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\"{details}}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n"
            ),
            reason = reason,
            details = details,
        );
        let events =
            decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, suffix));
        let failed = events.iter().find_map(|event| match event {
            AssistantEvent::Failed { message } => Some(message),
            _ => None,
        });
        let failed = failed.expect("failed Anthropic stop");
        assert_eq!(failed.finish.raw_provider_reason.as_deref(), Some(reason));
        assert_eq!(
            failed
                .finish
                .error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some(expected)
        );
    }
}

/// Architecture v2 part 2 §5.2 and §10.1; pinned Pi basis:
/// `packages/ai/test/anthropic-cache-write-1h-cost.test.ts` and
/// `packages/ai/src/models.ts:878-897` price the provider-reported one-hour
/// subset at twice the input rate and the remainder at the ordinary rate.
#[test]
fn anthropic_cache_write_one_hour_mixed_cost_matches_pi() {
    let model = anthropic_models()
        .expect("catalog")
        .into_iter()
        .find(|model| model.common.model_ref.model.as_str() == "claude-opus-4-8")
        .expect("Claude Opus 4.8");
    for (nested, expected_one_hour, expected_cost) in [
        (
            ",\"cache_creation\":{\"ephemeral_5m_input_tokens\":600000,\"ephemeral_1h_input_tokens\":400000}",
            Some(400_000),
            7_750_625,
        ),
        ("", None, 6_250_625),
    ] {
        let body = format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_cache\",\"usage\":{{\"input_tokens\":100,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":1000000{nested}}}}}}}\n\n",
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"Hi\"}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":100,\"output_tokens\":5,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":1000000}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n"
            ),
            nested = nested,
        );
        let events = decode_anthropic_messages_sse(
            body.as_bytes(),
            decode_context_for(&model, "cache-cost"),
        );
        let usage = &terminal_message(&events).usage;
        assert_eq!(usage.cache_write_tokens, Some(1_000_000));
        assert_eq!(usage.cache_write_one_hour_tokens, expected_one_hour);
        let cost = model
            .common
            .pricing
            .calculate_cost(usage, Currency::usd(), CacheWriteRetention::Default)
            .expect("cache pricing");
        assert_eq!(cost.micros, expected_cost);
    }
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:1035-1039` suppresses full-call
/// temperature whenever thinking is enabled or model compat disallows it.
#[test]
fn anthropic_full_options_temperature_suppression_matches_pi() {
    let (_, mut model, context, _) = parse_fixture("text-only");
    let encode = |model: &ModelDescriptor, thinking: AnthropicThinking| {
        let typed = typed_model(model);
        let ApiModelConfig::AnthropicMessages(config) = &model.api else {
            unreachable!()
        };
        let compat = AnthropicMessages::resolve_compat(&model.common.base_url, &config.compat)
            .expect("compat");
        let wire = AnthropicMessages::encode(
            EncodeContext {
                model: &typed,
                context: &context,
                compat: &compat,
                effective_base_url: &model.common.base_url,
            },
            &AnthropicOptions {
                max_tokens: 1_024,
                temperature: Some(0.75),
                thinking,
                thinking_display: AnthropicThinkingDisplay::Summarized,
                tool_choice: None,
                cache_retention: CacheRetention::None,
                metadata_user_id: None,
                interleaved_thinking: true,
            },
        )
        .expect("full options encode");
        serde_json::from_slice::<Value>(
            &OrderedJsonWriter::to_vec(&OrderedJsonValue::Object(wire)).expect("ordered JSON"),
        )
        .expect("full options JSON")
    };
    assert!(
        encode(
            &model,
            AnthropicThinking::Budget {
                budget_tokens: 2_048,
            },
        )
        .get("temperature")
        .is_none()
    );
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_temperature = Some(false);
    assert!(
        encode(&model, AnthropicThinking::Omitted)
            .get("temperature")
            .is_none()
    );
}

/// Architecture v2 part 2 §3.5 correction and §10.5; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:524-544,880-899` lets a full
/// call explicitly suppress the interleaved-thinking beta request header.
#[test]
fn anthropic_full_options_interleaved_thinking_controls_beta_header() {
    let (_, model, context, _) = parse_fixture("text-only");
    let response = read_bytes("text-only", "response-turn-1.sse");
    let transport = Arc::new(FixturePipelineTransport::new([response.clone(), response]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("full-options Anthropic registration");
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("full-options Anthropic Models");
    let mut options = AnthropicOptions {
        max_tokens: 1_024,
        temperature: Some(0.75),
        thinking: AnthropicThinking::Omitted,
        thinking_display: AnthropicThinkingDisplay::Summarized,
        tool_choice: None,
        cache_retention: CacheRetention::None,
        metadata_user_id: None,
        interleaved_thinking: false,
    };
    let _ = run_registered_full_options(&models, &model, context.clone(), options.clone());

    options.interleaved_thinking = true;
    let _ = run_registered_full_options(&models, &model, context, options);

    let requests = transport.requests.lock().expect("full-options requests");
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].headers.contains_key("anthropic-beta"));
    assert_eq!(
        requests[1]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("interleaved-thinking-2025-05-14")
    );
    let first: Value = serde_json::from_slice(&requests[0].body).expect("full-options body");
    assert_eq!(first["max_tokens"], 1_024);
    assert_eq!(first["temperature"], 0.75);
    assert!(first.get("thinking").is_none());
    assert_eq!(first["stream"], true);
}

/// Architecture v2 part 2 §3.3, §3.5, and §9.2; the local registered
/// full-options route applies the same `interleavedThinking` transport header
/// policy without invoking simple lowering.
#[test]
fn anthropic_full_options_interleaved_thinking_controls_beta_header_local() {
    let (_, model, context, _) = parse_fixture("text-only");
    let response = read_bytes("text-only", "response-turn-1.sse");
    let transport = Rc::new(LocalFixturePipelineTransport::new([
        response.clone(),
        response,
    ]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = LocalProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            local_anthropic_messages_api(Rc::clone(&transport) as Rc<dyn LocalHttpTransport>),
        )
        .build()
        .expect("local full-options Anthropic registration");
    let models = LocalModels::builder()
        .provider(registration)
        .build()
        .expect("local full-options Anthropic Models");
    let mut options = AnthropicOptions {
        max_tokens: 1_024,
        temperature: Some(0.75),
        thinking: AnthropicThinking::Omitted,
        thinking_display: AnthropicThinkingDisplay::Summarized,
        tool_choice: None,
        cache_retention: CacheRetention::None,
        metadata_user_id: None,
        interleaved_thinking: false,
    };
    let _ = run_registered_local_full_options(&models, &model, context.clone(), options.clone());
    options.interleaved_thinking = true;
    let _ = run_registered_local_full_options(&models, &model, context, options);

    let requests = transport.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].headers.contains_key("anthropic-beta"));
    assert_eq!(
        requests[1]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("interleaved-thinking-2025-05-14")
    );
    let first: Value = serde_json::from_slice(&requests[0].body).expect("local full-options body");
    assert_eq!(first["max_tokens"], 1_024);
    assert_eq!(first["temperature"], 0.75);
    assert!(first.get("thinking").is_none());
    assert_eq!(first["stream"], true);
}

/// Architecture v2 part 2 §3.5 and §10.5; pinned Pi basis:
/// `packages/ai/src/utils/deferred-tools.ts` and
/// `anthropic-messages.ts:973-1031,1118-1151,1333-1363` emit deferred tool
/// definitions and place `tool_reference` blocks apart from ordinary results.
#[test]
fn anthropic_deferred_tools_and_tool_references_match_pi() {
    let (fixture, mut model, mut context, simple) = parse_fixture("tool-results");
    let ApiModelConfig::AnthropicMessages(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_tool_references = Some(true);
    let mut deferred = context.tools[0].clone();
    deferred.name = "deferred_lookup".to_owned();
    deferred.description = "Deferred lookup".to_owned();
    context.tools.push(deferred);
    let Message::ToolResult(result) = &mut context.messages[2] else {
        panic!("fixture tool result")
    };
    result.added_tool_names = vec!["deferred_lookup".to_owned()];
    let body: Value = serde_json::from_slice(&encode_fixture(
        fixture["entrypoint"].as_str().expect("entrypoint"),
        &model,
        &context,
        &simple,
    ))
    .expect("deferred tool JSON");
    assert_eq!(body["tools"][1]["name"], "deferred_lookup");
    assert_eq!(body["tools"][1]["defer_loading"], true);
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        serde_json::json!([{"type":"tool_reference","tool_name":"deferred_lookup"}])
    );
    assert_eq!(
        body["messages"][2]["content"][1],
        serde_json::json!({"type":"text","text":"fixture file contents"})
    );
}

/// Architecture v2 part 2 §3.5 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:125-166` inserts the exact
/// placeholder text before an image-only tool result.
#[test]
fn anthropic_image_only_tool_result_has_pi_placeholder() {
    let (fixture, model, mut context, simple) = parse_fixture("tool-result-images");
    let Message::ToolResult(result) = &mut context.messages[2] else {
        panic!("fixture tool result")
    };
    result.content.remove(0);
    let body: Value = serde_json::from_slice(&encode_fixture(
        fixture["entrypoint"].as_str().expect("entrypoint"),
        &model,
        &context,
        &simple,
    ))
    .expect("image-only tool result JSON");
    assert_eq!(
        body["messages"][2]["content"][0]["content"][0],
        serde_json::json!({"type":"text","text":"(see attached image)"})
    );
}

/// Architecture v2 part 2 §3.5 and §10.8; pinned Pi basis:
/// `packages/ai/src/api/anthropic-messages.ts:880-970` composes beta variants,
/// defaults Pi's User-Agent, and lets explicit request headers win.
#[test]
fn anthropic_beta_and_user_agent_variants_match_pi() {
    let model = anthropic_models()
        .expect("catalog")
        .into_iter()
        .find(|model| model.common.model_ref.model.as_str() == "claude-opus-5")
        .expect("Claude Opus 5");
    let (_, fixture_model, _, _) = parse_fixture("text-only");
    let context = parse_context(
        &read_value("text-only", "canonical.json")["context"],
        &fixture_model,
    );
    let response = read_bytes("text-only", "response-turn-1.sse");
    let transport = Arc::new(FixturePipelineTransport::new([response.clone(), response]));
    let mut provider_headers = anthropic_default_headers();
    provider_headers.insert("x-api-key".into(), Some(FIXTURE_API_KEY.into()));
    let registration = ProviderRegistration::builder("anthropic")
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            AnthropicMessages::API_ID,
            anthropic_messages_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("fallback-beta registration");
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("fallback-beta Models");
    let _ = run_fixture_pipeline(
        &models,
        &model,
        context.clone(),
        SimpleGenerationOptions::default(),
    );
    let mut explicit = SimpleGenerationOptions::default();
    let explicit_beta = "claude-code-20250219,oauth-2025-04-20,custom-explicit-beta";
    explicit
        .headers
        .insert("anthropic-beta".into(), Some(explicit_beta.into()));
    explicit
        .headers
        .insert("user-agent".into(), Some("custom-client".into()));
    let _ = run_fixture_pipeline(&models, &model, context, explicit);
    let requests = transport.requests.lock().expect("header request lock");
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("server-side-fallback-2026-07-01")
    );
    #[cfg(target_arch = "wasm32")]
    let expected_user_agent = "pi (browser)";
    #[cfg(not(target_arch = "wasm32"))]
    let expected_user_agent_storage = anthropic_user_agent();
    #[cfg(not(target_arch = "wasm32"))]
    let expected_user_agent = expected_user_agent_storage.as_str();
    assert_eq!(
        requests[0]
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some(expected_user_agent)
    );
    assert_eq!(
        requests[1]
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some(explicit_beta)
    );
    assert_eq!(
        requests[1]
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some("custom-client")
    );
}

fn terminal_message(events: &[AssistantEvent]) -> &AssistantMessage {
    events
        .iter()
        .find_map(|event| match event {
            AssistantEvent::Finished { message } | AssistantEvent::Failed { message } => {
                Some(message)
            }
            _ => None,
        })
        .expect("terminal Anthropic message")
}

fn encode_case(case: &str) -> Value {
    let (fixture, model, context, simple) = parse_fixture(case);
    serde_json::from_slice(&encode_fixture(
        fixture["entrypoint"].as_str().expect("entrypoint"),
        &model,
        &context,
        &simple,
    ))
    .expect("wire JSON")
}

fn empty_signature_message() -> (ModelDescriptor, Context, AssistantMessage) {
    let (_, model, context, _) = parse_fixture("text-only");
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_empty\",\"model\":\"fixture-anthropic-model\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"visible thought\",\"signature\":\" \"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let output =
        decode_anthropic_messages_sse(body.as_bytes(), decode_context_for(&model, "empty"))
            .into_iter()
            .find_map(|event| match event {
                AssistantEvent::Finished { message } => Some(message),
                _ => None,
            })
            .expect("completed empty-signature response");
    (model, context, output)
}

fn decode_context_for(model: &ModelDescriptor, suffix: &str) -> AnthropicMessagesDecodeContext {
    AnthropicMessagesDecodeContext {
        message_id: MessageId::new(format!("fixture-response-{suffix}")),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
        tool_name_aliases: BTreeMap::new(),
    }
}

fn append_follow_up(context: &mut Context) {
    let index = context.messages.len();
    context.messages.push(Message::User(pi_ai::UserMessage {
        id: MessageId::new(format!("follow-up-{index}")),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("follow-up-block-{index}")),
            text: "continue".to_owned(),
        }],
        timestamp: Timestamp::from_unix_millis(FIXTURE_TIMESTAMP),
    }));
}

fn encode_context(model: &ModelDescriptor, context: &Context) -> Value {
    let simple = SimpleGenerationOptions {
        cache_retention: Some(CacheRetention::None),
        ..SimpleGenerationOptions::default()
    };
    serde_json::from_slice(&encode_fixture("stream", model, context, &simple)).expect("wire JSON")
}

#[derive(Debug)]
struct NeverTransport;

impl HttpTransport for NeverTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, pi_ai::TransportError>> {
        Box::pin(async {
            Err(pi_ai::TransportError::new(
                "never_transport",
                "fixture transport must not execute",
            ))
        })
    }
}

struct RecordingApiKeyInteraction {
    answers: Mutex<VecDeque<AuthAnswer>>,
    prompts: Mutex<Vec<AuthPrompt>>,
}

impl RecordingApiKeyInteraction {
    fn new(answers: impl IntoIterator<Item = AuthAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }
}

impl AuthInteraction for RecordingApiKeyInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        self.prompts.lock().expect("prompt lock").push(prompt);
        let answer = self.answers.lock().expect("answer lock").pop_front();
        Box::pin(async move {
            answer.ok_or_else(|| AuthInteractionError::Failed {
                code: "missing_test_answer".to_owned(),
                message: "test interaction has no remaining answer".to_owned(),
            })
        })
    }

    fn notify(&self, _event: AuthEvent) -> Result<(), AuthInteractionError> {
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async {
            Err(AuthInteractionError::Unsupported {
                message: "redirect receiver is not used by API-key login".to_owned(),
            })
        })
    }
}
