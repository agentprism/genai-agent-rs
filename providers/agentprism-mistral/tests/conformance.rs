//! Mistral §10.8 wire and stream conformance.

use agentprism_ai::*;
use agentprism_mistral::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

#[path = "../../fixtures/rust_support.rs"]
mod fixture;

fn typed_model(id: &str, reasoning: bool) -> TypedModelDescriptor<MistralConversations> {
    TypedModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("mistral", id),
            display_name: id.into(),
            base_url: Url::parse("https://api.mistral.ai").unwrap(),
            modalities: ModalityCapabilities {
                input: BTreeSet::from([Modality::Text, Modality::Image]),
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: 262_144,
                max_output_tokens: 4_096,
            },
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning,
            headers: HeaderMapSpec::new(),
        },
        config: MistralModelConfig::default(),
        extensions: ExtensionMap::new(),
    }
}

fn text_context() -> Context {
    let mut context = Context::new(Some("Be concise".into()));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("user-1"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("text-1"),
            text: "Hello".into(),
        }],
        timestamp: Timestamp::default(),
    }));
    context
}

#[test]
fn wire_mistral_conversations_basic_pi_exact() {
    // §10.8; Pi basis: api/mistral-conversations.ts buildChatPayload,
    // toMistralWirePayload, and toChatMessages.
    let model = typed_model("mistral-large-latest", false);
    let context = text_context();
    let options = MistralOptions {
        temperature: Some(0.25),
        max_tokens: Some(128),
        cache_retention: Some(CacheRetention::Short),
        session_id: Some("session-1".into()),
        ..Default::default()
    };
    let wire = encode_mistral_conversations(&model, &context, &options).unwrap();
    let body = OrderedJsonWriter::stringify(&wire.into()).unwrap();
    assert_eq!(
        body,
        r#"{"model":"mistral-large-latest","stream":true,"messages":[{"role":"system","content":"Be concise"},{"role":"user","content":"Hello"}],"temperature":0.25,"max_tokens":128,"prompt_cache_key":"session-1"}"#
    );
}

#[test]
fn mistral_tool_id_collision_uses_transcript_encounter_order_pi_exact() {
    // Architecture v2 part 2 §4.3 and §10.8; Pi basis:
    // packages/ai/src/api/mistral-conversations.ts:138-139,227-246 and
    // packages/ai/src/api/transform-messages.ts:76-156. The stateful
    // normalizer assigns a colliding candidate to the first transcript/block
    // occurrence, not to the lexicographically first original ID.
    let model = typed_model("mistral-large-latest", false);
    let mut context = Context::new(None);
    for (index, id) in ["ab-cdefghi", "a-bcdefghi"].into_iter().enumerate() {
        context.messages.push(Message::Assistant(AssistantMessage {
            id: MessageId::new(format!("foreign-assistant-{index}")),
            provider: ProviderId::new("foreign-provider"),
            api: ApiId::new("openai-responses"),
            requested_model: ModelId::new("foreign-model"),
            response_model: None,
            response_id: None,
            deferred: None,
            end_turn: None,
            diagnostics: Vec::new(),
            content: vec![ContentBlock::ToolCall {
                id: ContentBlockId::new(format!("foreign-tool-block-{index}")),
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: "lookup".into(),
                    arguments: json!({"index": index}),
                },
            }],
            replay: ReplayEnvelope::new(ReplayScope::new(
                "foreign-provider",
                "openai-responses",
                "foreign-model",
                "foreign-model",
            )),
            usage: Usage::zero(UsageSource::Unknown),
            cost: None,
            finish: AssistantFinish {
                reason: AssistantFinishReason::ToolUse,
                raw_provider_reason: Some("tool_calls".into()),
                error: None,
            },
            timestamp: Timestamp::default(),
        }));
        context
            .messages
            .push(Message::ToolResult(ToolResultMessage {
                id: MessageId::new(format!("foreign-result-{index}")),
                tool_call_id: ToolCallId::new(id),
                tool_name: "lookup".into(),
                content: vec![ToolResultContent::Text {
                    id: ContentBlockId::new(format!("foreign-result-text-{index}")),
                    text: format!("result-{index}"),
                }],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: Timestamp::default(),
            }));
    }

    let target = ModelDescriptor {
        common: model.common.clone(),
        api: ApiModelConfig::MistralConversations(model.config.clone()),
        extensions: model.extensions.clone(),
    };
    let projected = transform_context_for_model(
        &context,
        &target,
        &HandoffPolicy::default(),
        &MistralConversationsHandoff::default(),
    )
    .unwrap();
    assert_eq!(
        projected
            .report
            .changes
            .iter()
            .filter_map(|change| match change {
                HandoffChange::ToolCallIdRewritten { old, new, .. } => {
                    Some((old.as_str(), new.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![("ab-cdefghi", "abcdefghi"), ("a-bcdefghi", "mlj703uel")]
    );
    assert!(
        !projected
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::SyntheticToolResultInserted { .. }))
    );

    let wire = encode_mistral_conversations(&model, &projected.context, &MistralOptions::default())
        .unwrap();
    let value: Value =
        serde_json::from_slice(&OrderedJsonWriter::to_vec(&wire.into()).unwrap()).unwrap();
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages[0]["tool_calls"][0]["id"], "abcdefghi");
    assert_eq!(messages[1]["tool_call_id"], "abcdefghi");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "mlj703uel");
    assert_eq!(messages[3]["tool_call_id"], "mlj703uel");
}

#[test]
fn wire_mistral_conversations_ecmascript_trim_pi_exact() {
    // Architecture v2 part 2 §10.8; Pi basis:
    // packages/ai/src/api/mistral-conversations.ts `toChatMessages` and
    // `buildToolResultText`, whose JavaScript `trim()` includes U+FEFF but
    // excludes U+0085.
    let model = typed_model("mistral-large-latest", false);
    let mut context = Context::new(None);
    context.messages.push(Message::Assistant(AssistantMessage {
        id: MessageId::new("assistant-trim"),
        provider: ProviderId::new("mistral"),
        api: ApiId::new(MistralConversations::API_ID),
        requested_model: ModelId::new("mistral-large-latest"),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![
            ContentBlock::Text {
                id: ContentBlockId::new("feff-text"),
                text: "\u{feff}".into(),
            },
            ContentBlock::Thinking {
                id: ContentBlockId::new("u0085-thinking"),
                text: "\u{0085}".into(),
                redacted: false,
                replay_item: None,
            },
        ],
        replay: ReplayEnvelope::new(ReplayScope::new(
            "mistral",
            MistralConversations::API_ID,
            "mistral-large-latest",
            "mistral-large-latest",
        )),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::default(),
    }));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("tool-trim"),
            tool_call_id: ToolCallId::new("call12345"),
            tool_name: "lookup".into(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("tool-text"),
                text: "\u{feff}\u{0085}\u{feff}".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::default(),
        }));

    let wire = encode_mistral_conversations(&model, &context, &MistralOptions::default()).unwrap();
    let value: Value =
        serde_json::from_slice(&OrderedJsonWriter::to_vec(&wire.into()).unwrap()).unwrap();
    assert_eq!(value["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["messages"][0]["content"][0]["thinking"][0]["text"],
        "\u{0085}"
    );
    assert_eq!(value["messages"][1]["content"][0]["text"], "\u{0085}");
}

#[test]
fn wire_mistral_conversations_pi_exact() {
    // Architecture v2 part 2 §10.8; Pi basis: the complete captured
    // `api/mistral-conversations.ts` fixture matrix, including turn-two
    // assembly and a persistence round-trip.
    for case in fixture::family_cases(env!("CARGO_MANIFEST_DIR"), "mistral-conversations") {
        assert_captured_mistral_case(&case);
    }
}

fn assert_captured_mistral_case(case: &Path) {
    let canonical = fixture::canonical(case);
    let model = captured_mistral_model(&canonical["model"]);
    let mut context = fixture::context(
        &canonical["context"],
        &model.common,
        MistralConversations::API_ID,
    );
    let actual = encode_captured_mistral(&model, &context, &canonical);
    let expected = fs::read(case.join("request-turn-1.body.json")).expect("turn-one fixture");
    assert_eq!(
        actual,
        expected,
        "turn-one Mistral body mismatch for {}",
        case.file_name().unwrap().to_string_lossy()
    );

    let response = fs::read(case.join("response-turn-1.sse")).expect("response fixture");
    let assistant = decode_mistral_conversations_sse(
        &response,
        MistralConversationsDecodeContext {
            message_id: MessageId::new("fixture-turn-one-assistant"),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
            supports_finish_reason: true,
            grammar_tool_input_properties: Default::default(),
        },
    )
    .last()
    .and_then(AssistantEvent::terminal_message)
    .expect("terminal Mistral fixture assistant")
    .clone();
    let persisted = serde_json::to_vec(&assistant).expect("persist Mistral assistant");
    let assistant: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore Mistral assistant");
    context.messages.push(Message::Assistant(assistant));
    fixture::append_messages(
        &mut context,
        &canonical["turnTwoAppend"],
        &model.common,
        MistralConversations::API_ID,
    );
    let actual = encode_captured_mistral(&model, &context, &canonical);
    let expected = fs::read(case.join("request-turn-2.body.json")).expect("turn-two fixture");
    assert_eq!(
        actual,
        expected,
        "turn-two Mistral body mismatch for {}",
        case.file_name().unwrap().to_string_lossy()
    );
}

fn captured_mistral_model(value: &Value) -> ModelDescriptor {
    let level = |name: &str| {
        value["thinkingLevelMap"]
            .get(name)
            .and_then(Value::as_str)
            .map(|level| LevelSupport::Value(level.to_owned()))
    };
    ModelDescriptor {
        common: fixture::common_model(value),
        api: ApiModelConfig::MistralConversations(MistralModelConfig {
            thinking_levels: ThinkingLevelMap {
                off: level("off"),
                minimal: level("minimal"),
                low: level("low"),
                medium: level("medium"),
                high: level("high"),
                xhigh: level("xhigh"),
                max: level("max"),
            },
        }),
        extensions: Default::default(),
    }
}

fn encode_captured_mistral(
    model: &ModelDescriptor,
    context: &Context,
    canonical: &Value,
) -> Vec<u8> {
    let ApiModelConfig::MistralConversations(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<MistralConversations> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &MistralConversationsHandoff::default(),
    )
    .expect("Mistral fixture handoff")
    .context;
    let values = &canonical["options"];
    let options = if canonical["entrypoint"] == "streamSimple" {
        let simple = fixture::simple_options(values);
        let estimate = estimate_context_tokens(&projected).expect("Mistral fixture estimate");
        MistralConversations::lower_simple(
            SimpleLoweringContext {
                model: &typed,
                compat: &MistralCompat,
                effective_base_url: &model.common.base_url,
                estimated_input_tokens: estimate.tokens,
                available_context_tokens: model
                    .common
                    .limits
                    .context_window
                    .saturating_sub(estimate.tokens)
                    .saturating_sub(CONTEXT_SAFETY_TOKENS),
            },
            &simple,
            &MistralSimplePatch::default(),
        )
        .expect("lower Mistral fixture")
    } else {
        MistralOptions {
            temperature: values
                .get("temperature")
                .and_then(Value::as_f64)
                .map(|value| value as f32),
            max_tokens: values
                .get("maxTokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            cache_retention: values
                .get("cacheRetention")
                .and_then(Value::as_str)
                .map(fixture::cache_retention),
            session_id: values
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Default::default()
        }
    };
    let wire = MistralConversations::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &MistralCompat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("encode Mistral fixture");
    OrderedJsonWriter::to_vec(&wire.into()).expect("Mistral fixture wire")
}

#[test]
fn mistral_raw_stop_reason_pi_exact() {
    // Pi basis: packages/ai/test/mistral-raw-stop-reason.test.ts.
    let events = decode_mistral_conversations_sse(
        b"data: {\"id\":\"response-1\",\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"model_length\"}]}\n\n",
        MistralConversationsDecodeContext {
            message_id: MessageId::new("message-1"),
            provider: ProviderId::new("mistral"),
            requested_model: ModelId::new("mistral-large-latest"),
            timestamp: Timestamp::default(),
            supports_finish_reason: true,
            grammar_tool_input_properties: Default::default(),
        },
    );
    let message = events
        .iter()
        .find_map(|event| match event {
            AssistantEvent::Finished { message } => Some(message),
            _ => None,
        })
        .unwrap();
    assert_eq!(message.finish.reason, AssistantFinishReason::Length);
    assert_eq!(
        message.finish.raw_provider_reason.as_deref(),
        Some("model_length")
    );
}

#[test]
fn mistral_sse_ecmascript_trim_pi_exact() {
    // Architecture v2 part 2 §10.1 and §10.8; Pi basis:
    // packages/ai/src/api/mistral-conversations.ts `parseSSEStream`, whose
    // `trimStart()`/`trim()` include U+FEFF but exclude U+0085.
    let context = || MistralConversationsDecodeContext {
        message_id: MessageId::new("mistral-ecmascript-trim"),
        provider: ProviderId::new("mistral"),
        requested_model: ModelId::new("mistral-large-latest"),
        timestamp: Timestamp::default(),
        supports_finish_reason: true,
        grammar_tool_input_properties: Default::default(),
    };
    let event = r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#;
    let body = format!("data:\u{feff}{event}\u{feff}\n\n");
    let events = decode_mistral_conversations_sse(body.as_bytes(), context());
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Finished { .. })
    ));

    let body = format!("data:\u{0085}{event}\u{0085}\n\n");
    let events = decode_mistral_conversations_sse(body.as_bytes(), context());
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("U+0085 protocol-failure terminal message");
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert!(
        message
            .finish
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("invalid SSE JSON data"))
    );
}

#[test]
fn mistral_reasoning_mode_pi_exact() {
    // Pi basis: packages/ai/test/mistral-reasoning-mode.test.ts.
    for (id, expected_prompt, expected_effort) in [
        ("magistral-small", Some("reasoning"), None),
        ("mistral-small-latest", None, Some("high")),
        ("mistral-medium-3.5", None, Some("high")),
    ] {
        let model = typed_model(id, true);
        let options = MistralConversations::lower_simple(
            SimpleLoweringContext {
                model: &model,
                compat: &MistralCompat,
                effective_base_url: &model.common.base_url,
                estimated_input_tokens: 1,
                available_context_tokens: 10_000,
            },
            &SimpleGenerationOptions {
                reasoning: Some(ReasoningLevel::High),
                ..Default::default()
            },
            &MistralSimplePatch::default(),
        )
        .unwrap();
        assert_eq!(options.prompt_mode.as_deref(), expected_prompt);
        assert_eq!(options.reasoning_effort.as_deref(), expected_effort);
    }
}

#[test]
fn mistral_strict_tool_schema_pi_exact() {
    // Pi basis: packages/ai/test/mistral-tool-schema.test.ts.
    let model = typed_model("mistral-large-latest", false);
    let mut context = text_context();
    context.tools.push(ToolSpec {
        schema_version: 1,
        name: "weather".into(),
        description: "Get weather".into(),
        parameters: json!({"type":"object","properties":{"city":{"type":"string"}}}),
        constrained_sampling: Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: JsonSchemaStrictMode::Require,
            },
        )),
    });
    let wire = encode_mistral_conversations(&model, &context, &MistralOptions::default()).unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&OrderedJsonWriter::to_vec(&wire.into()).unwrap()).unwrap();
    assert_eq!(value["tools"][0]["function"]["strict"], true);
    assert_eq!(
        value["tools"][0]["function"]["parameters"]["additionalProperties"],
        false
    );
}

#[test]
fn mistral_catalog_matches_pinned_pi() {
    // Pi basis: providers/mistral.models.ts and published data/mistral.json.
    let models = mistral_models().unwrap();
    assert!(!models.is_empty());
    assert!(models.iter().all(|model| {
        model.common.model_ref.provider == ProviderId::new("mistral")
            && model.api.api_id() == ApiId::new("mistral-conversations")
    }));
}

struct DiagnosticTransport;

fn transport_diagnostic(kind: &str) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        schema_version: ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
        kind: kind.into(),
        timestamp: Timestamp::default(),
        error: None,
        details: Default::default(),
    }
}

impl HttpTransport for DiagnosticTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: vec![transport_diagnostic("mistral_response_recovery")],
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new("stream_failure", "body failed")
                        .with_provider_code("mistral_body")
                        .with_status(502)
                        .with_request_id("request-body")
                        .with_diagnostic(transport_diagnostic("mistral_body_recovery")))
                })),
            })
        })
    }
}

impl LocalHttpTransport for DiagnosticTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: vec![transport_diagnostic("mistral_response_recovery")],
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new("stream_failure", "body failed")
                        .with_provider_code("mistral_body")
                        .with_status(502)
                        .with_request_id("request-body")
                        .with_diagnostic(transport_diagnostic("mistral_body_recovery")))
                })),
            })
        })
    }
}

fn mistral_request(model: ModelDescriptor) -> ResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    ResolvedApiRequest {
        model,
        context: text_context(),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: Default::default(),
        endpoint,
        headers: http::HeaderMap::new(),
        auth_headers: http::HeaderMap::new(),
        api_key: None,
        api: ApiId::new(MistralConversations::API_ID),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_mistral_request(model: ModelDescriptor) -> LocalResolvedApiRequest {
    let request = mistral_request(model);
    LocalResolvedApiRequest {
        model: request.model,
        context: request.context,
        options: request.options,
        full_options: request.full_options,
        request_options: request.request_options,
        endpoint: request.endpoint,
        headers: request.headers,
        auth_headers: request.auth_headers,
        api_key: request.api_key,
        api: request.api,
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: request.retry_policy,
        timeout: request.timeout,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

fn mistral_descriptor() -> ModelDescriptor {
    let model = typed_model("mistral-large-latest", false);
    ModelDescriptor {
        common: model.common,
        api: ApiModelConfig::MistralConversations(model.config),
        extensions: model.extensions,
    }
}

fn assert_transport_diagnostics(message: &AssistantMessage) {
    let restored: AssistantMessage = serde_json::from_slice(
        &serde_json::to_vec(message).expect("persist Mistral diagnostic message"),
    )
    .expect("restore Mistral diagnostic message");
    assert_eq!(
        restored
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.kind.as_str())
            .collect::<Vec<_>>(),
        ["mistral_response_recovery", "mistral_body_recovery"]
    );
    let error = restored.finish.error.expect("transport failure");
    assert_eq!(error.code, "stream_failure");
    assert_eq!(error.provider_code.as_deref(), Some("mistral_body"));
    assert_eq!(error.status, Some(502));
    assert_eq!(error.request_id.as_deref(), Some("request-body"));
}

#[test]
fn mistral_transport_diagnostics_are_committed_send_and_local() {
    // Architecture v2 part 2 §2.1 and §9.2; Pi basis:
    // api/mistral-conversations.ts established-response stream commitment.
    let api = mistral_conversations_api(Arc::new(DiagnosticTransport));
    let mut stream = futures_executor::block_on(api.stream(
        mistral_request(mistral_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Send Mistral stream");
    let send_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_transport_diagnostics(
        send_events
            .last()
            .and_then(AssistantEvent::terminal_message)
            .expect("Send Mistral terminal"),
    );

    let api = local_mistral_conversations_api(Rc::new(DiagnosticTransport));
    let mut stream = futures_executor::block_on(api.stream(
        local_mistral_request(mistral_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Local Mistral stream");
    let local_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_transport_diagnostics(
        local_events
            .last()
            .and_then(AssistantEvent::terminal_message)
            .expect("Local Mistral terminal"),
    );
}

#[derive(Clone, Debug)]
struct CapturedMistralRequest {
    url: Url,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct CapturingMistralTransport {
    requests: Arc<Mutex<Vec<CapturedMistralRequest>>>,
    response: Arc<Vec<u8>>,
}

impl CapturingMistralTransport {
    fn terminal() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(
                b"data: {\"id\":\"response-transport\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
                    .to_vec(),
            ),
        }
    }

    fn with_response(response: impl Into<Vec<u8>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(response.into()),
        }
    }

    fn record(&self, request: &HttpRequest) {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedMistralRequest {
                url: request.url.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            });
    }

    fn last(&self) -> CapturedMistralRequest {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .expect("captured Mistral request")
            .clone()
    }
}

impl HttpTransport for CapturingMistralTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.record(&request);
        let response = Arc::clone(&self.response);
        Box::pin(async move {
            Ok(HttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                response.as_ref().clone(),
            ))
        })
    }
}

impl LocalHttpTransport for CapturingMistralTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.record(&request);
        let response = Arc::clone(&self.response);
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                response.as_ref().clone(),
            ))
        })
    }
}

fn collect_send(mut stream: AssistantStream) -> Vec<AssistantEvent> {
    futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    })
}

fn collect_local(mut stream: LocalAssistantStream) -> Vec<AssistantEvent> {
    futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    })
}

fn successful_send_events(
    transport: Arc<dyn HttpTransport>,
    request: ResolvedApiRequest,
) -> Vec<AssistantEvent> {
    let api = mistral_conversations_api(transport);
    let stream = futures_executor::block_on(api.stream(request, CancellationToken::new()))
        .expect("Send Mistral stream");
    collect_send(stream)
}

fn successful_local_events(
    transport: Rc<dyn LocalHttpTransport>,
    request: LocalResolvedApiRequest,
) -> Vec<AssistantEvent> {
    let api = local_mistral_conversations_api(transport);
    let stream = futures_executor::block_on(api.stream(request, CancellationToken::new()))
        .expect("Local Mistral stream");
    collect_local(stream)
}

#[test]
fn mistral_explicit_null_affinity_suppresses_auto_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “honors
    // case-insensitive header overrides and explicit affinity suppression”.
    let mut model = mistral_descriptor();
    model
        .common
        .headers
        .insert("X-Affinity".into(), Some("model-affinity".into()));

    let send_transport = CapturingMistralTransport::terminal();
    let mut request = mistral_request(model.clone());
    request.options.cache_retention = Some(CacheRetention::Short);
    request.options.session_id = Some("automatic-affinity".into());
    request.options.headers.insert("x-AFFINITY".into(), None);
    request.request_options = ApiRequestOptions::from(&request.options);
    request.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("custom-agent"),
    );
    successful_send_events(Arc::new(send_transport.clone()), request);
    let captured = send_transport.last();
    assert_eq!(
        captured.url.as_str(),
        "https://api.mistral.ai/v1/chat/completions"
    );
    assert!(!captured.headers.contains_key("x-affinity"));
    assert_eq!(captured.headers[http::header::USER_AGENT], "custom-agent");

    let local_transport = CapturingMistralTransport::terminal();
    let mut request = local_mistral_request(model);
    request.options.cache_retention = Some(CacheRetention::Short);
    request.options.session_id = Some("automatic-affinity".into());
    request.options.headers.insert("X-affinity".into(), None);
    request.request_options = ApiRequestOptions::from(&request.options);
    successful_local_events(Rc::new(local_transport.clone()), request);
    assert!(!local_transport.last().headers.contains_key("x-affinity"));
}

struct CamelCaseMistralTransform;

fn add_camel_case_mistral_fields(payload: &mut OrderedJsonObject) {
    payload.insert("topP", 0.9_f64);
    payload.insert("randomSeed", 42_u64);
    payload.insert("parallelToolCalls", true);
    payload.insert(
        "responseFormat",
        OrderedJsonValue::from(json!({
            "type": "json_schema",
            "jsonSchema": {
                "name": "result",
                "schemaDefinition": {"type": "object"}
            }
        })),
    );
    if let Some(OrderedJsonValue::Array(messages)) = payload.get_mut("messages") {
        messages.push(OrderedJsonValue::from(json!({
            "role": "tool",
            "toolCallId": "call-1",
            "content": [{
                "type": "image_url",
                "imageUrl": "data:image/png;base64,AA==",
                "referenceIds": ["reference-1"]
            }]
        })));
    }
}

impl PayloadTransform<MistralConversations> for CamelCaseMistralTransform {
    fn transform<'a>(
        &'a self,
        _context: PayloadTransformContext<'a, MistralConversations>,
        payload: &'a mut OrderedJsonObject,
    ) -> SendBoxFuture<'a, Result<PayloadTransformResult<OrderedJsonObject>, MiddlewareError>> {
        Box::pin(async move {
            add_camel_case_mistral_fields(payload);
            Ok(PayloadTransformResult::Continue)
        })
    }
}

impl LocalPayloadTransform<MistralConversations> for CamelCaseMistralTransform {
    fn transform<'a>(
        &'a self,
        _context: PayloadTransformContext<'a, MistralConversations>,
        payload: &'a mut OrderedJsonObject,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformResult<OrderedJsonObject>, MiddlewareError>>
    {
        Box::pin(async move {
            add_camel_case_mistral_fields(payload);
            Ok(PayloadTransformResult::Continue)
        })
    }
}

fn assert_mistral_camel_case_remapped(body: &[u8]) {
    let value: Value = serde_json::from_slice(body).expect("Mistral request JSON");
    assert_eq!(value["top_p"], json!(0.9));
    assert_eq!(value["random_seed"], json!(42));
    assert_eq!(value["parallel_tool_calls"], json!(true));
    assert_eq!(
        value["response_format"]["json_schema"]["schema"],
        json!({"type": "object"})
    );
    let message = value["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(message["tool_call_id"], "call-1");
    assert_eq!(
        message["content"][0]["image_url"],
        "data:image/png;base64,AA=="
    );
    assert_eq!(
        message["content"][0]["reference_ids"],
        json!(["reference-1"])
    );
    for camel_case in ["topP", "randomSeed", "parallelToolCalls", "responseFormat"] {
        assert!(value.get(camel_case).is_none(), "retained {camel_case}");
    }
}

#[test]
fn mistral_payload_transform_camel_case_fields_remap_send_and_local() {
    // Architecture v2 part 2 §2.6, §9.2, and §10.8; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “serializes SDK-style
    // payloads to the Mistral wire format”.
    let send_transport = CapturingMistralTransport::terminal();
    let mut request = mistral_request(mistral_descriptor());
    request.payload_transforms =
        Arc::from([
            Arc::new(PayloadTransformAdapter::<MistralConversations>::new(
                Arc::new(CamelCaseMistralTransform),
            )) as Arc<dyn ErasedPayloadTransform>,
        ]);
    successful_send_events(Arc::new(send_transport.clone()), request);
    assert_mistral_camel_case_remapped(&send_transport.last().body);

    let local_transport = CapturingMistralTransport::terminal();
    let mut request = local_mistral_request(mistral_descriptor());
    request.payload_transforms = Rc::from([Rc::new(LocalPayloadTransformAdapter::<
        MistralConversations,
    >::new(Rc::new(CamelCaseMistralTransform)))
        as Rc<dyn LocalErasedPayloadTransform>]);
    successful_local_events(Rc::new(local_transport.clone()), request);
    assert_mistral_camel_case_remapped(&local_transport.last().body);
}

#[derive(Clone)]
struct PendingMistralTransport;

impl HttpTransport for PendingMistralTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

impl LocalHttpTransport for PendingMistralTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

fn assert_mistral_timeout(events: &[AssistantEvent]) {
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("Mistral timeout terminal message");
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    let error = message.finish.error.as_ref().expect("timeout error");
    assert_eq!(error.code, "timeout");
    assert!(error.message.to_ascii_lowercase().contains("timed out"));
}

#[test]
fn mistral_timeout_covers_sse_body_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “applies the request
    // timeout while waiting for an SSE chunk”.
    let mut request = mistral_request(mistral_descriptor());
    request.timeout = Some(Duration::from_millis(5));
    request.request_options.timeout_ms = Some(5);
    assert_mistral_timeout(&successful_send_events(
        Arc::new(PendingMistralTransport),
        request,
    ));

    let mut request = local_mistral_request(mistral_descriptor());
    request.timeout = Some(Duration::from_millis(5));
    request.request_options.timeout_ms = Some(5);
    assert_mistral_timeout(&successful_local_events(
        Rc::new(PendingMistralTransport),
        request,
    ));
}

fn assert_mistral_cancelled(events: &[AssistantEvent]) {
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("Mistral cancellation terminal message");
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
}

#[test]
fn mistral_cancellation_while_waiting_for_sse_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “aborts while waiting
    // for an SSE chunk”.
    let cancellation = CancellationToken::new();
    let api = mistral_conversations_api(Arc::new(PendingMistralTransport));
    let stream = futures_executor::block_on(
        api.stream(mistral_request(mistral_descriptor()), cancellation.clone()),
    )
    .expect("Send Mistral stream");
    cancellation.cancel();
    assert_mistral_cancelled(&collect_send(stream));

    let cancellation = CancellationToken::new();
    let api = local_mistral_conversations_api(Rc::new(PendingMistralTransport));
    let stream = futures_executor::block_on(api.stream(
        local_mistral_request(mistral_descriptor()),
        cancellation.clone(),
    ))
    .expect("Local Mistral stream");
    cancellation.cancel();
    assert_mistral_cancelled(&collect_local(stream));
}

#[derive(Clone)]
struct MistralHttpFailureTransport;

impl HttpTransport for MistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                br#"{"message":"blocked by gateway"}"#.to_vec(),
            ))
        })
    }
}

impl LocalHttpTransport for MistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                br#"{"message":"blocked by gateway"}"#.to_vec(),
            ))
        })
    }
}

fn assert_mistral_http_failure(error: AiError) {
    assert_eq!(error.kind, AiErrorKind::Authorization);
    assert_eq!(error.status, Some(403));
    assert_eq!(
        error.message,
        r#"Mistral API error (403): {"message":"blocked by gateway"}"#
    );
}

#[test]
fn mistral_http_status_and_error_body_survive_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “preserves HTTP status
    // and response bodies in errors”.
    let api = mistral_conversations_api(Arc::new(MistralHttpFailureTransport));
    let mut request = mistral_request(mistral_descriptor());
    request.retry_classifier = Arc::new(MistralRetryClassifier::default());
    assert_mistral_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Send Mistral HTTP error"),
    );

    let api = local_mistral_conversations_api(Rc::new(MistralHttpFailureTransport));
    let mut request = local_mistral_request(mistral_descriptor());
    request.retry_classifier = Rc::new(LocalMistralRetryClassifier::default());
    assert_mistral_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Local Mistral HTTP error"),
    );
}

#[derive(Clone)]
struct EcmascriptMistralHttpFailureTransport;

impl HttpTransport for EcmascriptMistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                "\u{feff}blocked\u{0085}\u{feff}".as_bytes().to_vec(),
            ))
        })
    }
}

impl LocalHttpTransport for EcmascriptMistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                "\u{feff}blocked\u{0085}\u{feff}".as_bytes().to_vec(),
            ))
        })
    }
}

fn assert_mistral_ecmascript_http_failure(error: AiError) {
    assert_eq!(error.kind, AiErrorKind::Authorization);
    assert_eq!(error.status, Some(403));
    assert_eq!(error.message, "Mistral API error (403): blocked\u{0085}");
}

#[test]
fn mistral_http_error_body_ecmascript_trim_send_and_local_pi_exact() {
    // Architecture v2 part 2 §2.6, §9.2, and §10.8; Pi basis:
    // packages/ai/src/api/mistral-conversations.ts non-success handling uses
    // JavaScript `trim()` on the response body.
    let api = mistral_conversations_api(Arc::new(EcmascriptMistralHttpFailureTransport));
    let mut request = mistral_request(mistral_descriptor());
    request.retry_classifier = Arc::new(MistralRetryClassifier::default());
    assert_mistral_ecmascript_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Send Mistral ECMAScript-trim HTTP error"),
    );

    let api = local_mistral_conversations_api(Rc::new(EcmascriptMistralHttpFailureTransport));
    let mut request = local_mistral_request(mistral_descriptor());
    request.retry_classifier = Rc::new(LocalMistralRetryClassifier::default());
    assert_mistral_ecmascript_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Local Mistral ECMAScript-trim HTTP error"),
    );
}

#[derive(Clone)]
struct LongMistralHttpFailureTransport;

impl HttpTransport for LongMistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                "x".repeat(4_005).into_bytes(),
            ))
        })
    }
}

impl LocalHttpTransport for LongMistralHttpFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                403,
                http::HeaderMap::new(),
                "x".repeat(4_005).into_bytes(),
            ))
        })
    }
}

fn assert_mistral_truncated_http_failure(error: AiError) {
    assert_eq!(error.kind, AiErrorKind::Authorization);
    assert_eq!(error.status, Some(403));
    assert_eq!(
        error.message,
        format!(
            "Mistral API error (403): {}... [truncated 5 chars]",
            "x".repeat(4_000)
        )
    );
}

#[test]
fn mistral_http_error_body_truncates_at_4000_chars_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/src/api/mistral-conversations.ts, formatMistralError and
    // MAX_MISTRAL_ERROR_BODY_CHARS.
    let api = mistral_conversations_api(Arc::new(LongMistralHttpFailureTransport));
    let mut request = mistral_request(mistral_descriptor());
    request.retry_classifier = Arc::new(MistralRetryClassifier::default());
    assert_mistral_truncated_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Send Mistral HTTP error"),
    );

    let api = local_mistral_conversations_api(Rc::new(LongMistralHttpFailureTransport));
    let mut request = local_mistral_request(mistral_descriptor());
    request.retry_classifier = Rc::new(LocalMistralRetryClassifier::default());
    assert_mistral_truncated_http_failure(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Local Mistral HTTP error"),
    );
}

#[derive(Clone)]
struct PendingMistralErrorBodyTransport;

impl HttpTransport for PendingMistralErrorBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 403,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

impl LocalHttpTransport for PendingMistralErrorBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 403,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

fn assert_mistral_error_body_timeout(error: AiError) {
    assert_eq!(error.kind, AiErrorKind::Transport);
    assert_eq!(error.provider_code.as_deref(), Some("timeout"));
    assert!(error.message.to_ascii_lowercase().contains("timed out"));
}

#[test]
fn mistral_timeout_covers_error_body_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // api/mistral-conversations.ts keeps the fetch AbortSignal deadline alive
    // while awaiting `response.text()` for a non-2xx response.
    let api = mistral_conversations_api(Arc::new(PendingMistralErrorBodyTransport));
    let mut request = mistral_request(mistral_descriptor());
    request.timeout = Some(Duration::from_millis(5));
    request.request_options.timeout_ms = Some(5);
    assert_mistral_error_body_timeout(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Send Mistral error body must retain the request deadline"),
    );

    let api = local_mistral_conversations_api(Rc::new(PendingMistralErrorBodyTransport));
    let mut request = local_mistral_request(mistral_descriptor());
    request.timeout = Some(Duration::from_millis(5));
    request.request_options.timeout_ms = Some(5);
    assert_mistral_error_body_timeout(
        futures_executor::block_on(api.stream(request, CancellationToken::new()))
            .expect_err("Local Mistral error body must retain the request deadline"),
    );
}

#[derive(Clone)]
struct BytewiseMistralTransport {
    body: Arc<Vec<u8>>,
}

fn byte_chunks(body: &[u8]) -> Vec<Result<Vec<u8>, TransportError>> {
    let mut chunks = Vec::with_capacity(body.len());
    for byte in body {
        chunks.push(Ok(vec![*byte]));
    }
    chunks
}

impl HttpTransport for BytewiseMistralTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let chunks = byte_chunks(&self.body);
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::iter(chunks)),
            })
        })
    }
}

impl LocalHttpTransport for BytewiseMistralTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let chunks = byte_chunks(&self.body);
        Box::pin(async move {
            Ok(LocalHttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::iter(chunks)),
            })
        })
    }
}

fn assert_mistral_unicode(events: &[AssistantEvent]) {
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("bytewise Mistral terminal");
    assert!(matches!(
        message.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "héllo 🌍"
    ));
}

#[test]
fn mistral_bytewise_sse_utf8_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; Pi basis:
    // packages/ai/test/mistral-http-transport.test.ts, “parses SSE and UTF-8
    // sequences split across transport chunks”.
    let body = Arc::new(
        "data: {\"choices\":[{\"delta\":{\"content\":\"héllo 🌍\"},\"finish_reason\":\"stop\"}]}\r\n\r\n"
            .as_bytes()
            .to_vec(),
    );
    assert_mistral_unicode(&successful_send_events(
        Arc::new(BytewiseMistralTransport {
            body: Arc::clone(&body),
        }),
        mistral_request(mistral_descriptor()),
    ));
    assert_mistral_unicode(&successful_local_events(
        Rc::new(BytewiseMistralTransport { body }),
        local_mistral_request(mistral_descriptor()),
    ));
}

fn alternating_mistral_body() -> Vec<u8> {
    [
        json!({"choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"think-1"}]}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":[{"type":"text","text":"text-1"}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"id":"call-1","index":0,"function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"think-2"}]}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":[{"type":"text","text":"text-2"}]},"finish_reason":"tool_calls"}]}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>()
    .into_bytes()
}

fn assert_alternating_mistral_blocks(events: &[AssistantEvent]) {
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("alternating Mistral terminal");
    assert_eq!(message.content.len(), 5);
    assert!(
        matches!(&message.content[0], ContentBlock::Thinking { text, .. } if text == "think-1")
    );
    assert!(matches!(&message.content[1], ContentBlock::Text { text, .. } if text == "text-1"));
    assert!(
        matches!(&message.content[2], ContentBlock::ToolCall { call, .. } if call.id == ToolCallId::new("call-1") && call.name == "lookup")
    );
    assert!(
        matches!(&message.content[3], ContentBlock::Thinking { text, .. } if text == "think-2")
    );
    assert!(matches!(&message.content[4], ContentBlock::Text { text, .. } if text == "text-2"));

    let transitions = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ContentBlockStarted { kind, .. } => Some(("start", *kind)),
            AssistantEvent::ContentBlockFinished { block_id } => message
                .content
                .iter()
                .find(|block| block.id() == block_id)
                .map(|block| {
                    let kind = match block {
                        ContentBlock::Text { .. } => ContentBlockKind::Text,
                        ContentBlock::Thinking { .. } => ContentBlockKind::Thinking,
                        ContentBlock::ToolCall { .. } => ContentBlockKind::ToolCall,
                        ContentBlock::Image { .. } => panic!("assistant image block"),
                    };
                    ("finish", kind)
                }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        transitions,
        [
            ("start", ContentBlockKind::Thinking),
            ("finish", ContentBlockKind::Thinking),
            ("start", ContentBlockKind::Text),
            ("finish", ContentBlockKind::Text),
            ("start", ContentBlockKind::ToolCall),
            ("start", ContentBlockKind::Thinking),
            ("finish", ContentBlockKind::Thinking),
            ("start", ContentBlockKind::Text),
            ("finish", ContentBlockKind::Text),
            ("finish", ContentBlockKind::ToolCall),
        ]
    );
}

#[test]
fn mistral_content_block_switching_pi_order_send_and_local() {
    // Architecture v2 part 2 §1.3, §9.2, and §10.8; Pi basis:
    // api/mistral-conversations.ts `consumeChatStream` currentBlock switching.
    let transport = CapturingMistralTransport::with_response(alternating_mistral_body());
    let events = successful_send_events(Arc::new(transport), mistral_request(mistral_descriptor()));
    assert_alternating_mistral_blocks(&events);

    let transport = CapturingMistralTransport::with_response(alternating_mistral_body());
    let events = successful_local_events(
        Rc::new(transport),
        local_mistral_request(mistral_descriptor()),
    );
    assert_alternating_mistral_blocks(&events);
}
