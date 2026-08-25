use http::HeaderMap;
use pi_ai::{
    ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION, ApiFamily, ApiId, ApiModelConfig,
    ApiRequestOptions, AssistantEvent, AssistantFinish, AssistantFinishReason, AssistantMessage,
    AssistantMessageDiagnostic, CONTEXT_SAFETY_TOKENS, CacheRetention, CacheWriteRetentionPricing,
    CommonModelDescriptor, ConstrainedSampling, ConstrainedSamplingConfig, ContentBlock,
    ContentBlockId, Context, CustomApiModelConfig, DiagnosticErrorCode, DiagnosticErrorInfo,
    EncodeContext, ErasedApiFullOptions, ErasedApiHandler, HeaderMapSpec, JsonSchemaStrictMode,
    LevelSupport, Message, MessageId, Modality, ModalityCapabilities, ModelDescriptor, ModelLimits,
    ModelPricing, OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND,
    OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND, OPENAI_RESPONSES_REASONING_ITEM_KIND,
    OpenAiCodexReasoningSummary, OpenAiCodexResponses, OpenAiCodexResponsesOptions,
    OpenAiCodexResponsesSimplePatch, OpenAiCodexToolChoice, OpenAiResponses, OpenAiResponsesCompat,
    OpenAiResponsesHandoff, OpenAiResponsesModelConfig, OpenAiResponsesOptions,
    OpenAiResponsesReasoningSummary, OpenAiResponsesSimplePatch, OpenAiTextVerbosity,
    OpenAiThinkingValue, OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter, ReasoningLevel,
    ReplayCompleteness, ReplayEnvelope, ReplayItem, ReplayScope, SessionAffinityFormat,
    SimpleGenerationOptions, SimpleLoweringContext, ThinkingLevelMap, Timestamp, TokenPriceRates,
    ToolCall, ToolCallId, ToolChoice, ToolResultContent, ToolResultMessage, ToolSpec,
    TypedModelDescriptor, Usage, UsageSource, UserMessage, estimate_context_tokens,
    responses_grammar_tool_input_properties, transform_context_for_model,
};
use pi_ai_openai::{
    AzureOpenAiResponses, AzureOpenAiResponsesModelConfig, AzureOpenAiResponsesOptions,
    OpenAiResponsesDecodeContext, OpenAiResponsesHandler, OpenAiResponsesSseDecoder,
    decode_openai_responses_sse, normalize_azure_openai_base_url,
};
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use url::Url;

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/openai-responses.ts:buildParams`.
#[test]
fn wire_openai_responses_pi_exact() {
    assert_captured_responses_family("openai-responses");
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat = OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let mut context = user_context(Some("system"));
    context.tools.clear();
    let options = OpenAiResponsesOptions {
        max_output_tokens: Some(8),
        temperature: Some(0.25),
        sampling: OrderedJsonObject::from_iter([("top_p", OrderedJsonValue::from(0.9))]),
        reasoning_effort: Some("low".into()),
        reasoning_summary: Some(Some(OpenAiResponsesReasoningSummary::Auto)),
        service_tier: Some("flex".into()),
        tool_choice: Some(OrderedJsonValue::from("none")),
        cache_retention: CacheRetention::Short,
        session_id: Some("session-1".into()),
    };
    assert_eq!(
        encode_responses(&typed, &compat, &context, &options),
        br#"{"model":"gpt-5.4","input":[{"role":"developer","content":"system"},{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true,"prompt_cache_key":"session-1","store":false,"max_output_tokens":16,"temperature":0.25,"service_tier":"flex","tool_choice":"none","reasoning":{"effort":"low","summary":"auto"},"include":["reasoning.encrypted_content"],"top_p":0.9}"#.to_vec()
    );

    let fixture_model =
        responses_model("fixture-openai", "openai-responses", "fixture-openai-model");
    let fixture_typed = typed_responses(&fixture_model);
    let fixture_compat = OpenAiResponses::resolve_compat(
        &fixture_model.common.base_url,
        &fixture_typed.config.compat,
    )
    .unwrap();
    assert_eq!(
        encode_responses(
            &fixture_typed,
            &fixture_compat,
            &fixture_text_context(),
            &OpenAiResponsesOptions {
                max_output_tokens: None,
                ..default_responses_options()
            },
        ),
        captured_request("openai-responses", "text-only", 1)
    );
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/azure-openai-responses.ts:buildParams`.
#[test]
fn wire_azure_openai_responses_pi_exact() {
    assert_captured_responses_family("azure-openai-responses");
    let shared_model = responses_model("azure-openai-responses", "openai-responses", "gpt-5.4");
    let shared = typed_responses(&shared_model);
    let custom = AzureOpenAiResponsesModelConfig {
        responses: shared.config.clone(),
    };
    let typed = TypedModelDescriptor::<AzureOpenAiResponses> {
        common: shared.common,
        config: CustomApiModelConfig {
            api: ApiId::new("azure-openai-responses"),
            schema_version: 1,
            value: RawValue::from_string(serde_json::to_string(&custom).unwrap()).unwrap(),
        },
        extensions: shared.extensions,
    };
    let compat =
        AzureOpenAiResponses::resolve_compat(&typed.common.base_url, &custom.responses.compat)
            .unwrap();
    let wire = AzureOpenAiResponses::encode(
        EncodeContext {
            model: &typed,
            context: &user_context(Some("system")),
            compat: &compat,
            effective_base_url: &typed.common.base_url,
        },
        &AzureOpenAiResponsesOptions {
            responses: OpenAiResponsesOptions {
                max_output_tokens: Some(8),
                temperature: Some(0.25),
                sampling: OrderedJsonObject::new(),
                reasoning_effort: Some("low".into()),
                reasoning_summary: Some(Some(OpenAiResponsesReasoningSummary::Auto)),
                service_tier: None,
                tool_choice: None,
                cache_retention: CacheRetention::Short,
                session_id: Some("session-1".into()),
            },
            azure_base_url: None,
            azure_resource_name: None,
            deployment_name: "production-gpt".into(),
            api_version: "v1".into(),
        },
    )
    .unwrap();
    assert_eq!(
        OrderedJsonWriter::stringify(&wire.into()).unwrap(),
        r#"{"model":"production-gpt","input":[{"role":"developer","content":"system"},{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true,"prompt_cache_key":"session-1","store":false,"max_output_tokens":16,"temperature":0.25,"reasoning":{"effort":"low","summary":"auto"},"include":["reasoning.encrypted_content"]}"#
    );
}

#[test]
fn azure_openai_base_url_normalization_pi_exact() {
    // Pi basis: packages/ai/test/azure-openai-base-url.test.ts.
    for (source, expected) in [
        (
            "https://a.cognitiveservices.azure.com",
            "https://a.cognitiveservices.azure.com/openai/v1",
        ),
        (
            "https://a.ai.azure.com/openai/v1/responses",
            "https://a.ai.azure.com/openai/v1",
        ),
        (
            "https://proxy.example/v1?custom=true",
            "https://proxy.example/v1?custom=true",
        ),
    ] {
        assert_eq!(
            normalize_azure_openai_base_url(source)
                .unwrap()
                .as_str()
                .trim_end_matches('/'),
            expected
        );
    }
    assert_eq!(
        normalize_azure_openai_base_url(
            "\u{feff}https://a.openai.azure.com/openai/v1/responses\u{feff}"
        )
        .unwrap()
        .as_str()
        .trim_end_matches('/'),
        "https://a.openai.azure.com/openai/v1"
    );
    assert!(
        normalize_azure_openai_base_url(
            "\u{0085}https://a.openai.azure.com/openai/v1/responses\u{0085}"
        )
        .is_err()
    );
    assert!(normalize_azure_openai_base_url("not-a-url").is_err());
}

#[test]
fn azure_openai_tool_choice_pi_exact() {
    // Pi basis: packages/ai/test/azure-openai-tool-choice.test.ts.
    let mut shared_model = responses_model(
        "azure-openai-responses",
        "openai-responses",
        "test-deployment",
    );
    shared_model.common.reasoning = false;
    let shared = typed_responses(&shared_model);
    let custom = AzureOpenAiResponsesModelConfig {
        responses: shared.config.clone(),
    };
    let typed = TypedModelDescriptor::<AzureOpenAiResponses> {
        common: shared.common,
        config: CustomApiModelConfig {
            api: ApiId::new("azure-openai-responses"),
            schema_version: 1,
            value: RawValue::from_string(serde_json::to_string(&custom).unwrap()).unwrap(),
        },
        extensions: shared.extensions,
    };
    let compat =
        AzureOpenAiResponses::resolve_compat(&typed.common.base_url, &custom.responses.compat)
            .unwrap();
    let mut context = user_context(None);
    context.tools = vec![tool_spec("read")];
    let options = AzureOpenAiResponses::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compat,
            effective_base_url: &typed.common.base_url,
            estimated_input_tokens: 1,
            available_context_tokens: 1_000,
        },
        &SimpleGenerationOptions {
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        },
        &Default::default(),
    )
    .unwrap();
    let wire = AzureOpenAiResponses::encode(
        EncodeContext {
            model: &typed,
            context: &context,
            compat: &compat,
            effective_base_url: &typed.common.base_url,
        },
        &options,
    )
    .unwrap();
    assert_eq!(
        wire.get("tool_choice"),
        Some(&OrderedJsonValue::from("none"))
    );
    assert!(wire.get("tools").is_some());
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/openai-codex-responses.ts:buildRequestBody`.
#[test]
fn wire_openai_codex_responses_pi_exact() {
    assert_captured_responses_family("openai-codex-responses");
    let model = responses_model("openai-codex", "openai-codex-responses", "gpt-5.4");
    let typed = typed_codex(&model);
    let compat = OpenAiCodexResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let context = user_context(Some("system"));
    let options = OpenAiCodexResponsesOptions {
        temperature: Some(0.25),
        reasoning_effort: Some("high".into()),
        reasoning_summary: Some(Some(OpenAiCodexReasoningSummary::Concise)),
        service_tier: Some("priority".into()),
        text_verbosity: OpenAiTextVerbosity::Low,
        tool_choice: OpenAiCodexToolChoice::Auto,
        cache_retention: CacheRetention::Short,
        session_id: Some("session-1".into()),
    };
    let wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &typed,
            context: &context,
            compat: &compat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("encode Codex Responses");
    assert_eq!(
        OrderedJsonWriter::to_vec(&wire.into()).expect("wire"),
        br#"{"model":"gpt-5.4","store":false,"stream":true,"instructions":"system","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"text":{"verbosity":"low"},"include":["reasoning.encrypted_content"],"prompt_cache_key":"session-1","tool_choice":"auto","parallel_tool_calls":true,"temperature":0.25,"service_tier":"priority","reasoning":{"effort":"high","summary":"concise"}}"#.to_vec()
    );

    let fixture_model = responses_model(
        "openai-codex",
        "openai-codex-responses",
        "fixture-codex-model",
    );
    let fixture_typed = typed_codex(&fixture_model);
    let fixture_compat = OpenAiCodexResponses::resolve_compat(
        &fixture_model.common.base_url,
        &fixture_typed.config.compat,
    )
    .unwrap();
    let fixture_wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &fixture_typed,
            context: &fixture_text_context(),
            compat: &fixture_compat,
            effective_base_url: &fixture_model.common.base_url,
        },
        &OpenAiCodexResponsesOptions {
            temperature: None,
            reasoning_effort: None,
            reasoning_summary: None,
            service_tier: None,
            text_verbosity: OpenAiTextVerbosity::Low,
            tool_choice: OpenAiCodexToolChoice::Auto,
            cache_retention: CacheRetention::None,
            session_id: None,
        },
    )
    .unwrap();
    assert_eq!(
        OrderedJsonWriter::to_vec(&fixture_wire.into()).unwrap(),
        captured_request("openai-codex-responses", "text-only", 1)
    );
}

/// Architecture v2 part 2 §3/§10.8; pinned Pi basis:
/// `openai-codex-responses.ts:578-589` applies the model thinking-level map
/// to every fully typed effort and uses nullish fallback for a null mapping.
#[test]
fn responses_codex_full_reasoning_uses_model_map_pi_exact() {
    let mut model = responses_model("openai-codex", "openai-codex-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiCodexResponses(config) = &mut model.api else {
        unreachable!()
    };
    config.thinking_levels.minimal = Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
        "low".into(),
    )));
    config.thinking_levels.off = Some(LevelSupport::Unsupported);
    let typed = typed_codex(&model);
    let compat = OpenAiCodexResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let context = user_context(None);
    let mut options = OpenAiCodexResponsesOptions {
        temperature: None,
        reasoning_effort: Some("minimal".into()),
        reasoning_summary: None,
        service_tier: None,
        text_verbosity: OpenAiTextVerbosity::Low,
        tool_choice: OpenAiCodexToolChoice::Auto,
        cache_retention: CacheRetention::None,
        session_id: None,
    };
    let encode = |options: &OpenAiCodexResponsesOptions| {
        let wire = OpenAiCodexResponses::encode(
            EncodeContext {
                model: &typed,
                context: &context,
                compat: &compat,
                effective_base_url: &model.common.base_url,
            },
            options,
        )
        .expect("encode full Codex options");
        OrderedJsonWriter::to_vec(&wire.into()).expect("wire")
    };
    let minimal = String::from_utf8(encode(&options)).unwrap();
    assert!(minimal.contains(r#""reasoning":{"effort":"low","summary":"auto"}"#));

    options.reasoning_effort = Some("none".into());
    let unsupported_off = String::from_utf8(encode(&options)).unwrap();
    assert!(unsupported_off.contains(r#""reasoning":{"effort":"none","summary":"auto"}"#));

    let mut public_model = responses_model("openai", "openai-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiResponses(public_config) = &mut public_model.api else {
        unreachable!()
    };
    public_config.thinking_levels.medium = Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
        "mapped-medium".into(),
    )));
    let public_typed = typed_responses(&public_model);
    let public_compat =
        OpenAiResponses::resolve_compat(&public_model.common.base_url, &public_typed.config.compat)
            .unwrap();
    let public_wire: Value = serde_json::from_slice(&encode_responses(
        &public_typed,
        &public_compat,
        &context,
        &OpenAiResponsesOptions {
            reasoning_summary: Some(Some(OpenAiResponsesReasoningSummary::Auto)),
            ..default_responses_options()
        },
    ))
    .unwrap();
    assert_eq!(public_wire["reasoning"]["effort"], "medium");
}

/// Architecture v2 part 2 §3.1/§10.8; pinned Pi basis:
/// `openai-responses.ts:streamSimple`, `openai-codex-responses.ts:streamSimple`,
/// and both full encoders map the clamped level exactly once.
#[test]
fn responses_reasoning_level_map_applies_once_pi_exact() {
    let context = user_context(None);
    let simple = SimpleGenerationOptions {
        reasoning: Some(ReasoningLevel::Minimal),
        ..Default::default()
    };

    let mut public_model = responses_model("openai", "openai-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiResponses(public_config) = &mut public_model.api else {
        unreachable!()
    };
    public_config.thinking_levels.minimal = Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
        "low".into(),
    )));
    public_config.thinking_levels.low = Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
        "high".into(),
    )));
    let public_typed = typed_responses(&public_model);
    let public_compat =
        OpenAiResponses::resolve_compat(&public_model.common.base_url, &public_typed.config.compat)
            .unwrap();
    let public_options = OpenAiResponses::lower_simple(
        SimpleLoweringContext {
            model: &public_typed,
            compat: &public_compat,
            effective_base_url: &public_model.common.base_url,
            estimated_input_tokens: estimate_context_tokens(&context).unwrap().tokens,
            available_context_tokens: 100_000,
        },
        &simple,
        &OpenAiResponsesSimplePatch::default(),
    )
    .unwrap();
    assert_eq!(public_options.reasoning_effort.as_deref(), Some("minimal"));
    let public_wire: Value = serde_json::from_slice(&encode_responses(
        &public_typed,
        &public_compat,
        &context,
        &public_options,
    ))
    .unwrap();
    assert_eq!(public_wire["reasoning"]["effort"], "low");

    let mut codex_model = responses_model("openai-codex", "openai-codex-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiCodexResponses(codex_config) = &mut codex_model.api else {
        unreachable!()
    };
    codex_config.thinking_levels = public_typed.config.thinking_levels.clone();
    let codex_typed = typed_codex(&codex_model);
    let codex_compat = OpenAiCodexResponses::resolve_compat(
        &codex_model.common.base_url,
        &codex_typed.config.compat,
    )
    .unwrap();
    let codex_options = OpenAiCodexResponses::lower_simple(
        SimpleLoweringContext {
            model: &codex_typed,
            compat: &codex_compat,
            effective_base_url: &codex_model.common.base_url,
            estimated_input_tokens: estimate_context_tokens(&context).unwrap().tokens,
            available_context_tokens: 100_000,
        },
        &simple,
        &OpenAiCodexResponsesSimplePatch::default(),
    )
    .unwrap();
    assert_eq!(codex_options.reasoning_effort.as_deref(), Some("minimal"));
    let codex_wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &codex_typed,
            context: &context,
            compat: &codex_compat,
            effective_base_url: &codex_model.common.base_url,
        },
        &codex_options,
    )
    .unwrap();
    let codex_wire: Value =
        serde_json::from_slice(&OrderedJsonWriter::to_vec(&codex_wire.into()).unwrap()).unwrap();
    assert_eq!(codex_wire["reasoning"]["effort"], "low");
}

/// Architecture v2 part 2 §3.1/§10.8; pinned Pi basis:
/// `openai-responses.ts:streamSimple` and
/// `openai-codex-responses.ts:streamSimple` erase a clamped explicit `off`
/// before invoking their full encoders.
#[test]
fn responses_simple_explicit_off_is_absent_effort_pi_exact() {
    let context = user_context(None);
    let simple = SimpleGenerationOptions {
        reasoning: Some(ReasoningLevel::Off),
        ..Default::default()
    };

    let public_model = responses_model("openai", "openai-responses", "gpt-5.4");
    let public_typed = typed_responses(&public_model);
    let public_compat =
        OpenAiResponses::resolve_compat(&public_model.common.base_url, &public_typed.config.compat)
            .unwrap();
    let public_options = OpenAiResponses::lower_simple(
        SimpleLoweringContext {
            model: &public_typed,
            compat: &public_compat,
            effective_base_url: &public_model.common.base_url,
            estimated_input_tokens: estimate_context_tokens(&context).unwrap().tokens,
            available_context_tokens: 100_000,
        },
        &simple,
        &OpenAiResponsesSimplePatch::default(),
    )
    .unwrap();
    assert!(public_options.reasoning_effort.is_none());
    let public_wire: Value = serde_json::from_slice(&encode_responses(
        &public_typed,
        &public_compat,
        &context,
        &public_options,
    ))
    .unwrap();
    assert_eq!(
        public_wire["reasoning"],
        serde_json::json!({"effort":"none"})
    );
    assert!(public_wire["reasoning"].get("summary").is_none());
    assert!(public_wire.get("include").is_none());

    let codex_model = responses_model("openai-codex", "openai-codex-responses", "gpt-5.4");
    let codex_typed = typed_codex(&codex_model);
    let codex_compat = OpenAiCodexResponses::resolve_compat(
        &codex_model.common.base_url,
        &codex_typed.config.compat,
    )
    .unwrap();
    let codex_options = OpenAiCodexResponses::lower_simple(
        SimpleLoweringContext {
            model: &codex_typed,
            compat: &codex_compat,
            effective_base_url: &codex_model.common.base_url,
            estimated_input_tokens: estimate_context_tokens(&context).unwrap().tokens,
            available_context_tokens: 100_000,
        },
        &simple,
        &OpenAiCodexResponsesSimplePatch::default(),
    )
    .unwrap();
    assert!(codex_options.reasoning_effort.is_none());
    let codex_wire = OpenAiCodexResponses::encode(
        EncodeContext {
            model: &codex_typed,
            context: &context,
            compat: &codex_compat,
            effective_base_url: &codex_model.common.base_url,
        },
        &codex_options,
    )
    .unwrap();
    let codex_wire: Value =
        serde_json::from_slice(&OrderedJsonWriter::to_vec(&codex_wire.into()).unwrap()).unwrap();
    assert!(codex_wire.get("reasoning").is_none());
}

/// Architecture v2 part 2 §2.6/§3.6; full-options compatibility is resolved
/// from the effective post-authentication endpoint, not the catalog URL.
#[test]
fn responses_full_options_affinity_uses_effective_base_url() {
    let model = responses_model("gateway", "openai-responses", "gpt-5.4");
    assert!(!model.common.base_url.as_str().contains("openrouter.ai"));
    let options = ErasedApiFullOptions::new::<OpenAiResponses>(OpenAiResponsesOptions {
        cache_retention: CacheRetention::Short,
        session_id: Some("effective-session".into()),
        ..default_responses_options()
    });
    let effective = Url::parse("https://openrouter.ai/api/v1").unwrap();
    let mut headers = HeaderMap::new();
    ErasedApiHandler::apply_full_options_headers(
        &OpenAiResponsesHandler::default(),
        &model,
        &user_context(None),
        &options,
        &effective,
        &ApiRequestOptions::default(),
        &mut headers,
    )
    .unwrap();
    assert_eq!(headers["x-session-id"], "effective-session");
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-client-request-id").is_none());
}

/// Architecture v2 part 2 §1.6/§10.2; exact index-less SSE shape from
/// `packages/ai/test/openai-codex-stream.test.ts:buildSSEPayload` at the
/// pinned Pi commit. JavaScript keys these slots with `undefined`; Rust
/// assigns the observed provider ordinal while preserving the same message.
#[test]
fn responses_codex_indexless_streaming_fixture_matches_pi() {
    let fixture = br#"data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}

data: {"type":"response.content_part.added","part":{"type":"output_text","text":""}}

data: {"type":"response.output_text.delta","delta":"Hello"}

data: {"type":"response.output_item.done","item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello"}]}}

data: {"type":"response.completed","response":{"status":"completed","end_turn":false,"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{"cached_tokens":0}}}}

"#;
    let mut context = decode_context();
    context.provider = "openai-codex".into();
    context.api = "openai-codex-responses".into();
    context.requested_model = "gpt-5.1-codex".into();
    let events = decode_openai_responses_sse(fixture, context);
    let message = terminal(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert_eq!(message.end_turn, Some(false));
    assert_eq!(message.usage.input_tokens, 5);
    assert_eq!(message.usage.output_tokens, 3);
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::TextDelta { delta, .. } if delta == "Hello"
    )));
    assert!(matches!(
        message.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "Hello"
    ));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `response.created` in `openai-responses-shared.ts`.
#[test]
fn responses_response_id_survives_round_trip() {
    assert_eq!(decoded_message().response_id.as_deref(), Some("resp_1"));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: reasoning-item
/// `JSON.stringify` replay in `openai-responses-shared.ts`.
#[test]
fn responses_reasoning_item_preserves_full_json() {
    let message = decoded_message();
    let item = replay(&message, OPENAI_RESPONSES_REASONING_ITEM_KIND);
    assert_eq!(
        item.json_bytes().expect("reasoning JSON"),
        br#"{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"content":[],"encrypted_content":"cipher","status":"completed"}"#
    );
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: encrypted reasoning is
/// included for stateless turn-two replay.
#[test]
fn responses_reasoning_encrypted_content_survives() {
    let bytes = replay(&decoded_message(), OPENAI_RESPONSES_REASONING_ITEM_KIND)
        .json_bytes()
        .expect("reasoning JSON")
        .to_vec();
    assert!(
        std::str::from_utf8(&bytes)
            .expect("UTF-8")
            .contains(r#""encrypted_content":"cipher""#)
    );
}

/// Pinned Pi basis: `azure-openai-responses-reasoning-replay.test.ts`;
/// terminal response output supplies encrypted reasoning omitted by item.done.
#[test]
fn responses_terminal_backfills_encrypted_reasoning() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_azure","model":"gpt-5.4"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_azure","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_azure","type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"content":[],"status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_azure","model":"gpt-5.4","status":"completed","output":[{"id":"rs_azure","type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"content":[],"encrypted_content":"terminal-cipher","status":"completed"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#,
        decode_context(),
    );
    let payload = replay_json(terminal(&events), OPENAI_RESPONSES_REASONING_ITEM_KIND);
    assert_eq!(payload["encrypted_content"], "terminal-cipher");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: `output_index` slots.
#[test]
fn responses_output_items_preserve_global_order() {
    let message = decoded_message();
    assert_eq!(
        message
            .replay
            .items
            .iter()
            .map(|item| item.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: text signature ID.
#[test]
fn responses_text_item_id_survives() {
    let payload = replay_json(&decoded_message(), OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND);
    assert_eq!(payload["id"], "msg_1");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: text signature phase.
#[test]
fn responses_text_phase_survives() {
    let payload = replay_json(&decoded_message(), OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND);
    assert_eq!(payload["phase"], "final_answer");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: function `call_id`.
#[test]
fn responses_function_call_call_id_survives() {
    let payload = replay_json(
        &decoded_message(),
        OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND,
    );
    assert_eq!(payload["call_id"], "call_1");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: function output-item ID.
#[test]
fn responses_function_call_item_id_survives() {
    let payload = replay_json(
        &decoded_message(),
        OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND,
    );
    assert_eq!(payload["item_id"], "fc_1");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: deferred-call namespace.
#[test]
fn responses_function_call_namespace_survives() {
    let payload = replay_json(
        &decoded_message(),
        OPENAI_RESPONSES_FUNCTION_CALL_IDENTITY_KIND,
    );
    assert_eq!(payload["namespace"], "dynamic_tools");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: paired `fc_*` IDs are
/// omitted when replaying a different model on the same provider/API.
#[test]
fn responses_different_model_drops_paired_item_id() {
    let model = responses_model("openai", "openai-responses", "new-model");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::Assistant(assistant_tool_message(
            "openai",
            "openai-responses",
            "old-model",
            "call_1|fc_paired",
        )));
    let wire: serde_json::Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .unwrap();
    assert!(wire["input"][0].get("id").is_none());
    assert_eq!(wire["input"][0]["call_id"], "call_1");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `openai-responses-shared.ts:convertResponsesMessages` uses only the first
/// two pipe-separated tool-call ID components.
#[test]
fn responses_tool_call_compound_id_uses_first_two_parts_pi_exact() {
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::Assistant(assistant_tool_message(
            "openai",
            "openai-responses",
            "gpt-5.4",
            "call_1|fc_1|ignored",
        )));
    let wire: Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .unwrap();
    assert_eq!(wire["input"][0]["call_id"], "call_1");
    assert_eq!(wire["input"][0]["id"], "fc_1");
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:convertResponsesMessages` preserves a deferred
/// tool namespace across models while dropping its paired `fc_*` item ID.
#[test]
fn responses_cross_model_deferred_namespace_survives_pi_exact() {
    let mut model = responses_model("openai", "openai-responses", "new-model");
    let ApiModelConfig::OpenAiResponses(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_additional_tools = Some(true);
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context.tools.push(tool_spec("read_file"));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("load-result"),
            tool_call_id: ToolCallId::new("loader"),
            tool_name: "load_tools".into(),
            content: Vec::new(),
            details: None,
            usage: None,
            added_tool_names: vec!["read_file".into()],
            is_error: false,
            timestamp: Timestamp::from_unix_millis(1),
        }));
    context.messages.push(Message::Assistant(decoded_message()));
    let projected = transform_context_for_model(
        &context,
        &model,
        &Default::default(),
        &OpenAiResponsesHandoff,
    )
    .expect("cross-model Responses projection")
    .context;
    let wire: Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &projected,
        &default_responses_options(),
    ))
    .unwrap();
    let input = wire["input"].as_array().unwrap();
    let sequence = input
        .iter()
        .map(|item| match item["type"].as_str() {
            Some("function_call_output") => {
                format!("function_call_output:{}", item["call_id"].as_str().unwrap())
            }
            Some("additional_tools") => format!(
                "additional_tools:{}",
                item["tools"][0]["name"].as_str().unwrap()
            ),
            Some("message") => format!("message:{}", item["content"][0]["text"].as_str().unwrap()),
            Some("function_call") => {
                format!("function_call:{}", item["name"].as_str().unwrap())
            }
            other => panic!("unexpected Responses input item: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequence,
        [
            "function_call_output:loader",
            "additional_tools:read_file",
            "message:think",
            "message:answer",
            "function_call:read_file",
            "function_call_output:call_1",
        ]
    );
    let call = &input[4];
    assert!(call.get("id").is_none());
    assert_eq!(call["namespace"], "dynamic_tools");
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `buildForeignResponsesItemId`.
#[test]
fn responses_foreign_function_item_id_is_normalized() {
    let model = responses_model("openai", "openai-responses", "target");
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::Assistant(assistant_tool_message(
            "foreign",
            "openai-responses",
            "source",
            "call!bad|foreign/item/id",
        )));
    let projected = transform_context_for_model(
        &context,
        &model,
        &Default::default(),
        &OpenAiResponsesHandoff,
    )
    .expect("handoff")
    .context;
    let Message::Assistant(message) = &projected.messages[0] else {
        unreachable!()
    };
    let ContentBlock::ToolCall { call, .. } = &message.content[0] else {
        unreachable!()
    };
    let (call_id, item_id) = call.id.as_str().split_once('|').expect("compound ID");
    assert_eq!(call_id, "call_bad");
    assert!(item_id.starts_with("fc_"));
    assert!(item_id.len() <= 64);
}

/// Architecture v2 part 2 §1.3/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream`. An output item starts
/// before its canonical block, remains incomplete when the request fails, and
/// is never encoded into the next request.
#[test]
fn responses_incomplete_output_item_is_not_replayed() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_incomplete","model":"gpt-5.4"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_partial","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"partial"}

"#,
        decode_context(),
    );
    let message = terminal(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(
        message.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    );
    let replay_started = events
        .iter()
        .position(|event| matches!(event, AssistantEvent::ReplayItemStarted { .. }))
        .expect("replay item start");
    let content_started = events
        .iter()
        .position(|event| matches!(event, AssistantEvent::ContentBlockStarted { .. }))
        .expect("content block start");
    assert!(replay_started < content_started);

    // Keep the partial message visible to the family encoder so this assertion
    // isolates replay completeness from failed-turn projection.
    let mut replay_candidate = message.clone();
    replay_candidate.finish = AssistantFinish {
        reason: AssistantFinishReason::Length,
        raw_provider_reason: Some("incomplete.max_output_tokens".into()),
        error: None,
    };
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat = OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let mut context = Context::new(None);
    context.messages.push(Message::Assistant(replay_candidate));
    let body = encode_responses(&typed, &compat, &context, &default_responses_options());
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("Responses request");
    assert_eq!(wire["input"], serde_json::json!([]));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis: turn-two response input
/// walks canonical content order and reuses each surviving replay identity in
/// place (`openai-responses-shared.ts:218–293`).
#[test]
fn responses_turn_two_input_items_match_pi_order() {
    let bytes = turn_two_wire();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("wire JSON");
    let kinds = value["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("message")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "message",
            "reasoning",
            "message",
            "function_call",
            "function_call_output"
        ]
    );
}

/// Architecture v2 part 2 §10.2/§10.8; pinned Pi basis:
/// `openai-responses-reasoning-replay-e2e.test.ts`.
#[test]
fn openai_responses_encrypted_reasoning_turn_two_pi_exact() {
    assert_eq!(
        turn_two_wire(),
        br#"{"model":"gpt-5.4","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]},{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"content":[],"encrypted_content":"cipher","status":"completed"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}],"status":"completed","id":"msg_1","phase":"final_answer"},{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}","namespace":"dynamic_tools"},{"type":"function_call_output","call_id":"call_1","output":"contents"}],"stream":true,"store":false,"max_output_tokens":128000,"reasoning":{"effort":"none"}}"#.to_vec()
    );
}

/// Architecture v2 part 2 §1.6/§1.9 R6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:218-293` consults each thinking block's own
/// `thinkingSignature` while walking canonical block order.
#[test]
fn responses_reasoning_replay_stays_on_exact_content_block_pi_exact() {
    let mut message = decoded_message();
    let signed_reasoning = message.content.remove(0);
    let text = message.content.remove(0);
    message.content = vec![
        ContentBlock::Thinking {
            id: ContentBlockId::new("unsigned-thinking"),
            text: "unsigned".into(),
            redacted: false,
            replay_item: None,
        },
        text,
        signed_reasoning,
    ];
    message.replay.items.retain(|item| {
        matches!(
            item.kind.as_str(),
            OPENAI_RESPONSES_REASONING_ITEM_KIND | OPENAI_RESPONSES_MESSAGE_IDENTITY_KIND
        )
    });
    message.finish.reason = AssistantFinishReason::Stop;

    let persisted = serde_json::to_vec(&message).expect("persist assistant message");
    let restored: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore assistant message");
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context.messages.push(Message::Assistant(restored));
    let wire: Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .expect("Responses request");
    let input = wire["input"].as_array().expect("input array");

    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["content"][0]["text"], "answer");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["id"], "rs_1");
}

/// Pinned Pi basis: `openai-responses-empty-tool-result.test.ts`.
#[test]
fn responses_empty_tool_result_placeholder_matches_pi() {
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::Assistant(assistant_tool_message(
            "openai",
            "openai-responses",
            "gpt-5.4",
            "call_1",
        )));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("result"),
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: "read_file".into(),
            content: Vec::new(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::from_unix_millis(2),
        }));
    let wire: serde_json::Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .unwrap();
    assert_eq!(wire["input"][1]["output"], "(no tool output)");
}

/// Pinned Pi basis: `openai-responses-tool-result-images.test.ts`.
#[test]
fn responses_tool_result_images_match_pi() {
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("result"),
            tool_call_id: ToolCallId::new("call_1"),
            tool_name: "view".into(),
            content: vec![ToolResultContent::Image {
                id: ContentBlockId::new("image"),
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::from_unix_millis(2),
        }));
    let wire: serde_json::Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .unwrap();
    assert_eq!(
        wire["input"][0]["output"][0],
        serde_json::json!({
            "type":"input_image", "detail":"auto", "image_url":"data:image/png;base64,aGVsbG8="
        })
    );
}

/// Pinned Pi basis: `openai-responses-partial-json-cleanup.test.ts`.
#[test]
fn responses_partial_json_scratch_is_not_persisted() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"README"}

data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"bad","message":"failed"}}}

"#,
        decode_context(),
    );
    let encoded = serde_json::to_value(terminal(&events)).expect("persisted message");
    let text = encoded.to_string();
    assert!(!text.contains("partialJson"));
    assert!(!text.contains("customInput"));
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` recognizes only
/// `response.completed` and `response.incomplete`, while
/// `openai-codex-responses.ts:mapCodexEventToResponsesEvent` additionally
/// maps Codex-only `response.done` to completion.
#[test]
fn responses_response_done_is_codex_only() {
    let public = decode_openai_responses_sse(
        br#"data: {"type":"response.done","response":{"id":"resp_done","model":"gpt-5.4","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}

"#,
        decode_context(),
    );
    let public = terminal(&public);
    assert_eq!(public.response_id, None);
    assert_eq!(public.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        public.finish.error.as_ref().unwrap().message,
        "OpenAI Responses stream ended before a terminal response event"
    );

    let mut codex_context = decode_context();
    codex_context.provider = "openai-codex".into();
    codex_context.api = "openai-codex-responses".into();
    let codex = decode_openai_responses_sse(
        br#"data: {"type":"response.done","response":{"id":"resp_done","model":"gpt-5.4","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}

"#,
        codex_context,
    );
    let message = terminal(&codex);
    assert_eq!(message.response_id.as_deref(), Some("resp_done"));
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert_eq!(
        message.finish.raw_provider_reason.as_deref(),
        Some("completed")
    );
}

/// Architecture v2 part 1 §3.9 and part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses.ts:serviceTierMultiplier/calculateCost` applies flex at
/// one-half, priority at two, and GPT-5.5 priority at five-halves.
#[test]
fn responses_openai_service_tier_cost_pi_exact() {
    for (model, tier, expected_micros) in [
        ("gpt-5.4", "flex", 1_500_000),
        ("gpt-5.4", "priority", 6_000_000),
        ("gpt-5.5", "priority", 7_500_000),
    ] {
        let mut context = priced_decode_context("openai-responses", model);
        context.requested_service_tier = Some("default".into());
        let sse = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_cost\",\"status\":\"completed\",\"service_tier\":\"{tier}\",\"output\":[],\"usage\":{{\"input_tokens\":1000000,\"output_tokens\":1000000,\"total_tokens\":2000000}}}}}}\n\n"
        );
        let events = decode_openai_responses_sse(sse.as_bytes(), context);
        let cost = terminal(&events).cost.as_ref().expect("public cost");
        assert_eq!(cost.currency.as_str(), "USD");
        assert_eq!(cost.micros, expected_micros, "{model} {tier}");
    }
}

/// Architecture v2 part 1 §3.9 and part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-codex-stream.test.ts` verifies that an echoed `default` tier is
/// resolved back to the client-requested flex/priority tier before pricing.
#[test]
fn responses_codex_service_tier_cost_pi_exact() {
    for (model, requested_tier, expected_micros) in [
        ("gpt-5.1-codex", "flex", 1_500_000),
        ("gpt-5.1-codex", "priority", 6_000_000),
        ("gpt-5.5", "flex", 1_500_000),
        ("gpt-5.5", "priority", 7_500_000),
    ] {
        let mut context = priced_decode_context("openai-codex-responses", model);
        context.provider = "openai-codex".into();
        context.requested_service_tier = Some(requested_tier.into());
        let events = decode_openai_responses_sse(
            br#"data: {"type":"response.completed","response":{"id":"resp_cost","status":"completed","service_tier":"default","output":[],"usage":{"input_tokens":1000000,"output_tokens":1000000,"total_tokens":2000000,"input_tokens_details":{"cached_tokens":0}}}}

"#,
            context,
        );
        let persisted = serde_json::to_vec(terminal(&events)).expect("persist priced response");
        let restored: AssistantMessage =
            serde_json::from_slice(&persisted).expect("restore priced response");
        let cost = restored.cost.as_ref().expect("Codex cost");
        assert_eq!(cost.currency.as_str(), "USD");
        assert_eq!(cost.micros, expected_micros, "{model} {requested_tier}");
    }
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-codex-responses.ts:mapCodexEventToResponsesEvent` validates Codex
/// status values with `ResponseStatus.safeParse`, dropping unknown values,
/// while public Responses passes its status through to the exhaustive shared
/// finalizer.
#[test]
fn responses_codex_unknown_status_normalizes_to_stop_only_for_codex() {
    let public = decode_openai_responses_sse(
        br#"data: {"type":"response.completed","response":{"id":"resp_public","model":"gpt-5.4","status":"future_status","output":[]}}

"#,
        decode_context(),
    );
    let public = terminal(&public);
    assert_eq!(public.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        public.finish.raw_provider_reason.as_deref(),
        Some("future_status")
    );
    assert_eq!(
        public.finish.error.as_ref().unwrap().message,
        "Unhandled stop reason: future_status"
    );

    let mut codex_context = decode_context();
    codex_context.provider = "openai-codex".into();
    codex_context.api = "openai-codex-responses".into();
    let codex = decode_openai_responses_sse(
        br#"data: {"type":"response.done","response":{"id":"resp_codex","model":"gpt-5.4","status":"future_status","output":[]}}

"#,
        codex_context,
    );
    let codex = terminal(&codex);
    assert_eq!(codex.finish.reason, AssistantFinishReason::Stop);
    assert_eq!(codex.finish.raw_provider_reason, None);
    assert_eq!(codex.finish.error, None);
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-codex-responses.ts:parseSseJsonEvents` dispatches only frames ended
/// by an empty line and drops its remaining buffer at EOF. The public
/// Responses family retains its existing tail-dispatch behavior.
#[test]
fn responses_codex_unterminated_sse_tail_is_not_dispatched() {
    let tail = br#"data: {"type":"response.completed","response":{"id":"resp_tail","model":"gpt-5.4","status":"completed","output":[]}}"#;
    let public = decode_openai_responses_sse(tail, decode_context());
    assert_eq!(terminal(&public).finish.reason, AssistantFinishReason::Stop);
    assert_eq!(terminal(&public).response_id.as_deref(), Some("resp_tail"));

    let mut codex_context = decode_context();
    codex_context.provider = "openai-codex".into();
    codex_context.api = "openai-codex-responses".into();
    let codex = decode_openai_responses_sse(tail, codex_context);
    let codex = terminal(&codex);
    assert_eq!(codex.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        codex.finish.error.as_ref().unwrap().message,
        "OpenAI Responses stream ended before a terminal response event"
    );
    assert_eq!(codex.response_id, None);
}

/// Architecture v2 part 2 §1.6/§10.8; pinned Pi basis:
/// `openai-responses-shared.ts:convertResponsesMessages` increments
/// `msgIndex` only after a message emits wire input. Skipped empty user and
/// thinking-only assistant messages do not perturb later fallback IDs.
#[test]
fn responses_fallback_message_ids_skip_unencoded_messages_pi_exact() {
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("empty-user"),
        content: Vec::new(),
        timestamp: Timestamp::from_unix_millis(1),
    }));
    context.messages.push(Message::Assistant(AssistantMessage {
        id: MessageId::new("thinking-only"),
        provider: "anthropic".into(),
        api: "anthropic-messages".into(),
        requested_model: "claude".into(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Thinking {
            id: ContentBlockId::new("unsigned-thinking"),
            text: "private".into(),
            redacted: false,
            replay_item: None,
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(
            "anthropic",
            "anthropic-messages",
            "claude",
            "claude",
        )),
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(2),
    }));
    context.messages.push(Message::Assistant(AssistantMessage {
        id: MessageId::new("visible-assistant"),
        provider: "anthropic".into(),
        api: "anthropic-messages".into(),
        requested_model: "claude".into(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("visible-text"),
            text: "visible".into(),
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(
            "anthropic",
            "anthropic-messages",
            "claude",
            "claude",
        )),
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(3),
    }));

    let wire: Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .unwrap();
    assert_eq!(wire["input"].as_array().unwrap().len(), 1);
    assert_eq!(wire["input"][0]["id"], "msg_pi_0");
}

/// Architecture v2 part 2 §10.2 and the §1.6 correction; pinned Pi basis:
/// `openai-responses-shared.ts` initializes function arguments from
/// `output_item.added` and treats the final item as authoritative.
#[test]
fn responses_tool_arguments_retain_initial_and_authoritative_final() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"old\":"}}

data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"true}"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"new\":false}","status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_args","model":"gpt-5.4","status":"completed","output":[]}}

"#,
        decode_context(),
    );
    let argument_events = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ToolArgumentsDelta { delta, .. } => Some(("delta", delta.as_str())),
            AssistantEvent::ToolArgumentsReplaced { arguments, .. } => {
                Some(("replace", arguments.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        argument_events,
        vec![
            ("delta", r#"{"old":"#),
            ("delta", "true}"),
            ("replace", r#"{"new":false}"#)
        ]
    );
    let ContentBlock::ToolCall { call, .. } = &terminal(&events).content[0] else {
        unreachable!()
    };
    assert_eq!(call.arguments, serde_json::json!({"new": false}));
}

/// Architecture v2 part 2 §10.2 and the §1.6 correction; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` assigns the final
/// reasoning and message item text even when it is not a streamed prefix.
#[test]
fn responses_authoritative_text_and_thinking_replacement() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"draft thought"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"authoritative thought"}],"content":[],"status":"completed"}}

data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[],"status":"in_progress"}}

data: {"type":"response.output_text.delta","output_index":1,"delta":"draft answer"}

data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"final answer","annotations":[]}],"status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_replace","model":"gpt-5.4","status":"completed","output":[]}}

"#,
        decode_context(),
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ThinkingReplaced { thinking, .. }
            if thinking == "authoritative thought"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::TextReplaced { text, .. } if text == "final answer"
    )));
    assert!(matches!(
        &terminal(&events).content[..],
        [
            ContentBlock::Thinking { text: thinking, .. },
            ContentBlock::Text { text, .. }
        ] if thinking == "authoritative thought" && text == "final answer"
    ));
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` falls back to streamed
/// reasoning, function arguments, and custom input when item-done omits them.
#[test]
fn responses_item_done_omissions_preserve_streamed_values_pi_exact() {
    let mut context = decode_context();
    context
        .grammar_tool_input_properties
        .insert("query".into(), "payload".into());
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"streamed thought"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"content":[],"status":"completed"}}

data: {"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"README.md\"}"}

data: {"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","status":"completed"}}

data: {"type":"response.output_item.added","output_index":2,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_2","name":"query","input":""}}

data: {"type":"response.custom_tool_call_input.delta","output_index":2,"delta":"hello"}

data: {"type":"response.output_item.done","output_index":2,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_2","name":"query","status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}

"#,
        context,
    );
    let message = terminal(&events);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking { text, .. } if text == "streamed thought"
    ));
    let ContentBlock::ToolCall { call: function, .. } = &message.content[1] else {
        unreachable!()
    };
    assert_eq!(function.arguments, serde_json::json!({"path":"README.md"}));
    let ContentBlock::ToolCall { call: custom, .. } = &message.content[2] else {
        unreachable!()
    };
    assert_eq!(custom.arguments, serde_json::json!({"payload":"hello"}));
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` deletes every output
/// slot at `response.output_item.done`, so deltas and duplicate done events
/// arriving afterward are ignored for all supported slot kinds.
#[test]
fn responses_post_completion_deltas_are_ignored() {
    let mut context = decode_context();
    context
        .grammar_tool_input_properties
        .insert("query".into(), "payload".into());
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thought"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"content":[],"status":"completed"}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":" ignored"}

data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}

data: {"type":"response.output_text.delta","output_index":1,"delta":"answer"}

data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],"status":"completed"}}

data: {"type":"response.output_text.delta","output_index":1,"delta":" ignored"}

data: {"type":"response.output_item.added","output_index":2,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"README.md\"}"}

data: {"type":"response.output_item.done","output_index":2,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","status":"completed"}}

data: {"type":"response.function_call_arguments.delta","output_index":2,"delta":" ignored"}

data: {"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"late\":true}"}

data: {"type":"response.output_item.added","output_index":3,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_2","name":"query","input":""}}

data: {"type":"response.custom_tool_call_input.delta","output_index":3,"delta":"hello"}

data: {"type":"response.output_item.done","output_index":3,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_2","name":"query","status":"completed"}}

data: {"type":"response.custom_tool_call_input.delta","output_index":3,"delta":" ignored"}

data: {"type":"response.custom_tool_call_input.done","output_index":3,"input":"late"}

data: {"type":"response.completed","response":{"id":"resp_late","status":"completed","output":[]}}

"#,
        context,
    );
    let message = terminal(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::ToolUse);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking { text, .. } if text == "thought"
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::Text { text, .. } if text == "answer"
    ));
    let ContentBlock::ToolCall { call: function, .. } = &message.content[2] else {
        unreachable!()
    };
    assert_eq!(function.arguments, serde_json::json!({"path":"README.md"}));
    let ContentBlock::ToolCall { call: custom, .. } = &message.content[3] else {
        unreachable!()
    };
    assert_eq!(custom.arguments, serde_json::json!({"payload":"hello"}));
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-responses-shared.ts` does not copy `response.model` into the
/// assistant message, and falsy terminal encrypted content is not backfilled.
#[test]
fn responses_metadata_and_empty_reasoning_backfill_match_pi() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[],"content":[],"status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_1","model":"provider-alias","status":"completed","output":[{"id":"rs_1","type":"reasoning","encrypted_content":""}]}}

"#,
        decode_context(),
    );
    let message = terminal(&events);
    assert!(message.response_model.is_none());
    let replay = message
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == OPENAI_RESPONSES_REASONING_ITEM_KIND)
        .unwrap();
    let payload: Value = serde_json::from_slice(replay.json_bytes().unwrap()).unwrap();
    assert!(payload.get("encrypted_content").is_none());
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `openai-responses-shared.ts` uses JavaScript template interpolation for
/// top-level errors and accepts a failed event without a response object.
#[test]
fn responses_error_missing_fields_match_pi() {
    let top_level =
        decode_openai_responses_sse(b"data: {\"type\":\"error\"}\n\n", decode_context());
    assert_eq!(
        terminal(&top_level).finish.error.as_ref().unwrap().message,
        "Error Code undefined: undefined"
    );

    let failed = decode_openai_responses_sse(
        b"data: {\"type\":\"response.failed\"}\n\n",
        decode_context(),
    );
    assert_eq!(
        terminal(&failed).finish.error.as_ref().unwrap().message,
        "Unknown error (no error details in response)"
    );
}

/// Architecture v2 part 2 §10.2 and the §1.6 correction; pinned Pi basis:
/// `openai-codex-responses.ts:mapCodexEvents` copies `response.end_turn` onto
/// the completed assistant message.
#[test]
fn responses_codex_end_turn_metadata_survives_round_trip() {
    let mut context = decode_context();
    context.provider = "openai-codex".into();
    context.api = "openai-codex-responses".into();
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.done","response":{"id":"resp_end_turn","model":"gpt-5.4","status":"completed","end_turn":false,"output":[]}}

"#,
        context,
    );
    let persisted = serde_json::to_vec(terminal(&events)).expect("persist Codex end-turn metadata");
    let restored: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore Codex end-turn metadata");
    assert_eq!(restored.end_turn, Some(false));
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-codex-responses.ts:717-748` copies `response.end_turn` only while
/// mapping a terminal done/completed/incomplete event. `response.created` is
/// handled by `openai-responses-shared.ts:598-600` and captures only the ID.
#[test]
fn responses_codex_created_end_turn_is_ignored_without_terminal_end_turn_pi_exact() {
    let mut context = decode_context();
    context.provider = "openai-codex".into();
    context.api = "openai-codex-responses".into();
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_created","end_turn":true}}

data: {"type":"response.done","response":{"id":"resp_created","status":"completed","output":[]}}

"#,
        context,
    );

    let message = terminal(&events);
    assert_eq!(message.response_id.as_deref(), Some("resp_created"));
    assert_eq!(message.end_turn, None);
}

/// Architecture v2 part 2 §1.6; pinned Pi basis:
/// `utils/diagnostics.ts:addAssistantDiagnostic` and the Codex pre-stream
/// WebSocket-to-SSE fallback retain a redacted diagnostic on the message.
#[test]
fn responses_fallback_diagnostic_survives_round_trip() {
    let mut context = decode_context();
    context.provider = "openai-codex".into();
    context.api = "openai-codex-responses".into();
    let mut decoder = OpenAiResponsesSseDecoder::new(context);
    decoder
        .add_diagnostic(AssistantMessageDiagnostic {
            schema_version: ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
            kind: "provider_transport_failure".into(),
            timestamp: Timestamp::from_unix_millis(1),
            error: Some(DiagnosticErrorInfo {
                name: Some("TransportError".into()),
                message: "connect failed".into(),
                stack: None,
                code: Some(DiagnosticErrorCode::String("connect".into())),
            }),
            details: BTreeMap::from([
                (
                    "configuredTransport".into(),
                    serde_json::json!("websocket-cached"),
                ),
                ("fallbackTransport".into(), serde_json::json!("sse")),
                ("eventsEmitted".into(), serde_json::json!(false)),
            ]),
        })
        .unwrap();
    let mut events = decoder.take_events();
    events.extend(decoder.push(
        br#"data: {"type":"response.done","response":{"id":"resp_diag","model":"gpt-5.4","status":"completed","output":[]}}

"#,
    ));
    events.extend(decoder.finish());
    let persisted = serde_json::to_vec(terminal(&events)).expect("persist diagnostic");
    let restored: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore diagnostic");
    assert_eq!(restored.diagnostics.len(), 1);
    assert_eq!(restored.diagnostics[0].kind, "provider_transport_failure");
    assert_eq!(
        restored.diagnostics[0].details["configuredTransport"],
        "websocket-cached"
    );
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `openai-responses-shared.ts:finalizeResponse` and its `response.failed`
/// branch retain the provider's raw status details on failed messages.
#[test]
fn responses_raw_stop_reasons_survive_failures() {
    let incomplete = decode_openai_responses_sse(
        br#"data: {"type":"response.incomplete","response":{"id":"resp_filtered","model":"gpt-5.4","status":"incomplete","incomplete_details":{"reason":"content_filter"}}}

"#,
        decode_context(),
    );
    assert_eq!(
        terminal(&incomplete).finish.raw_provider_reason.as_deref(),
        Some("incomplete.content_filter")
    );

    let failed = decode_openai_responses_sse(
        br#"data: {"type":"response.failed","response":{"id":"resp_failed","model":"gpt-5.4","status":"failed","error":{"code":"bad","message":"denied"}}}

"#,
        decode_context(),
    );
    assert_eq!(
        terminal(&failed).finish.raw_provider_reason.as_deref(),
        Some("failed")
    );
}

/// Architecture v2 part 2 §10.1 `stream_response_id_is_preserved`; pinned Pi
/// basis: `openai-responses-shared.ts:744-754` does not consume
/// `response.failed.response.id`.
#[test]
fn responses_failed_only_does_not_record_response_id_pi_exact() {
    let failed = decode_openai_responses_sse(
        br#"data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"bad","message":"denied"}}}

"#,
        decode_context(),
    );

    assert_eq!(terminal(&failed).response_id, None);
}

/// Architecture v2 part 2 §10.1 `stream_response_id_is_preserved`; pinned Pi
/// basis: `openai-responses-shared.ts:598-600,744-754` records the created ID
/// and does not overwrite it from a later failed event.
#[test]
fn responses_failed_does_not_overwrite_created_response_id_pi_exact() {
    let failed = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_created"}}

data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"bad","message":"denied"}}}

"#,
        decode_context(),
    );

    let failed = terminal(&failed);
    assert_eq!(failed.response_id.as_deref(), Some("resp_created"));
    let error = failed.finish.error.as_ref().expect("public failure");
    assert_eq!(error.message, "bad: denied");
    assert_eq!(error.request_id.as_deref(), Some("resp_created"));
}

/// Architecture v2 part 2 §1.6/§10.1; pinned Pi basis:
/// `openai-codex-responses.ts:704-749` maps Codex error events before the
/// public Responses decoder sees them.
#[test]
fn responses_codex_error_mapping_matches_pi() {
    let mut context = decode_context();
    context.provider = "openai-codex".into();
    context.api = "openai-codex-responses".into();

    let failed = decode_openai_responses_sse(
        br#"data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"bad","message":"denied"}}}

"#,
        context.clone(),
    );
    let failed = terminal(&failed);
    assert_eq!(failed.finish.raw_provider_reason, None);
    assert_eq!(failed.finish.error.as_ref().unwrap().message, "denied");
    assert_eq!(
        failed
            .finish
            .error
            .as_ref()
            .unwrap()
            .provider_code
            .as_deref(),
        Some("bad")
    );

    let top_level = decode_openai_responses_sse(
        br#"data: {"type":"error","error":{"code":"nested_bad","message":"nested denied"}}

"#,
        context,
    );
    let top_level = terminal(&top_level);
    assert_eq!(top_level.finish.raw_provider_reason, None);
    assert_eq!(
        top_level.finish.error.as_ref().unwrap().message,
        "Codex error: nested denied"
    );
    assert_eq!(
        top_level
            .finish
            .error
            .as_ref()
            .unwrap()
            .provider_code
            .as_deref(),
        Some("nested_bad")
    );
}

/// Architecture v2 part 2 §1.6/§3; pinned Pi basis: public Responses accepts
/// `auto|detailed|concise`, while Codex additionally accepts `off|on`.
#[test]
fn responses_reasoning_summary_domains_match_pi() {
    assert!(serde_json::from_str::<OpenAiResponsesReasoningSummary>(r#""off""#).is_err());
    assert_eq!(
        serde_json::from_str::<OpenAiCodexReasoningSummary>(r#""off""#).unwrap(),
        OpenAiCodexReasoningSummary::Off
    );
    assert_eq!(
        serde_json::from_str::<OpenAiCodexReasoningSummary>(r#""on""#).unwrap(),
        OpenAiCodexReasoningSummary::On
    );
}

/// Pinned Pi basis: `constrained-sampling.test.ts`; grammar custom-tool
/// argument fragments concatenate into one append-only JSON object.
#[test]
fn responses_grammar_custom_input_fragments_are_append_only_json() {
    let mut context = decode_context();
    context
        .grammar_tool_input_properties
        .insert("query".into(), "payload".into());
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_1","name":"query","input":""}}

data: {"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"hel"}

data: {"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"lo"}

data: {"type":"response.custom_tool_call_input.done","output_index":0,"input":"hello"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_1","name":"query","input":"hello","status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_custom","model":"gpt-5.4","status":"completed"}}

"#,
        context,
    );
    let deltas = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ToolArgumentsDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(deltas, r#"{"payload":"hello"}"#);
    let ContentBlock::ToolCall { call, .. } = &terminal(&events).content[0] else {
        unreachable!()
    };
    assert_eq!(call.arguments, serde_json::json!({"payload":"hello"}));
}

/// Pinned Pi basis: `openai-responses-terminal-event.test.ts`.
#[test]
fn responses_terminal_event_is_required() {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_missing"}}

"#,
        decode_context(),
    );
    assert_eq!(
        terminal(&events).finish.reason,
        AssistantFinishReason::Error
    );
    assert!(
        terminal(&events)
            .finish
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("terminal response event")
    );
}

/// Pinned Pi basis: `openai-responses-cache-affinity-e2e.test.ts`.
#[test]
fn responses_session_affinity_headers_match_pi() {
    let compat = OpenAiResponsesCompat {
        session_affinity_format: Some(pi_ai::SessionAffinityFormat::OpenAi),
        ..Default::default()
    };
    let mut headers = HeaderMap::new();
    pi_ai::apply_openai_responses_full_headers(
        &compat,
        &OpenAiResponsesOptions {
            cache_retention: CacheRetention::Short,
            session_id: Some("session-1".into()),
            ..default_responses_options()
        },
        &mut headers,
    )
    .expect("affinity headers");
    assert_eq!(headers["session_id"], "session-1");
    assert_eq!(headers["x-client-request-id"], "session-1");
}

/// Pinned Pi basis: `openai-codex-responses.ts:buildSSEHeaders`.
#[test]
fn responses_codex_session_affinity_headers_match_pi() {
    let long_session_id = "x".repeat(80);
    assert_eq!(
        pi_ai::openai_codex_responses_transport_session_id(
            CacheRetention::Short,
            Some(&long_session_id),
        )
        .as_deref(),
        Some(long_session_id.as_str()),
        "Pi keys WebSocket continuation and sticky fallback by the raw typed option"
    );
    let mut headers = HeaderMap::new();
    pi_ai::apply_openai_codex_responses_full_headers(
        &OpenAiCodexResponsesOptions {
            temperature: None,
            reasoning_effort: None,
            reasoning_summary: Some(Some(OpenAiCodexReasoningSummary::Auto)),
            service_tier: None,
            text_verbosity: OpenAiTextVerbosity::Low,
            tool_choice: OpenAiCodexToolChoice::Auto,
            cache_retention: CacheRetention::Short,
            session_id: Some(long_session_id),
        },
        &mut headers,
    )
    .expect("affinity headers");
    let clamped_session_id = "x".repeat(64);
    assert_eq!(headers["session-id"], clamped_session_id);
    assert_eq!(headers["x-client-request-id"], clamped_session_id);
    assert!(headers.get("session_id").is_none());
}

/// Pinned Pi basis: `openai-responses-compat.test.ts`.
#[test]
fn responses_compat_defaults_and_overrides_match_pi() {
    let compat = OpenAiResponses::resolve_compat(
        &Url::parse("https://openrouter.ai/api/v1").unwrap(),
        &OpenAiResponsesCompat {
            supports_developer_role: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(compat.supports_developer_role, Some(false));
    assert_eq!(
        compat.session_affinity_format,
        Some(pi_ai::SessionAffinityFormat::OpenRouter)
    );
}

/// Pinned Pi basis: `deferred-tools.test.ts`; Responses prefers its native
/// `additional_tools` marker when both deferred-loading modes are supported.
#[test]
fn responses_deferred_tools_use_additional_tools() {
    let mut model = responses_model("openai", "openai-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiResponses(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_additional_tools = Some(true);
    config.compat.supports_tool_search = Some(true);
    let typed = typed_responses(&model);
    let compat = OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let context = deferred_tools_context();
    let wire: serde_json::Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .expect("wire JSON");

    assert_eq!(wire["tools"].as_array().unwrap().len(), 1);
    assert_eq!(wire["tools"][0]["name"], "base_tool");
    let marker = wire["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "additional_tools")
        .expect("additional_tools marker");
    assert_eq!(marker["role"], "developer");
    assert_eq!(marker["tools"][0]["name"], "late_tool");
    assert!(marker["tools"][0].get("defer_loading").is_none());
}

/// Pinned Pi basis: `deferred-tools.test.ts`; client tool search is the
/// fallback when native `additional_tools` is unavailable.
#[test]
fn responses_deferred_tools_fall_back_to_tool_search() {
    let mut model = responses_model("openai-proxy", "openai-responses", "gpt-5.4");
    let ApiModelConfig::OpenAiResponses(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_additional_tools = Some(false);
    config.compat.supports_tool_search = Some(true);
    let typed = typed_responses(&model);
    let compat = OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
        .expect("compat");
    let context = deferred_tools_context();
    let wire: serde_json::Value = serde_json::from_slice(&encode_responses(
        &typed,
        &compat,
        &context,
        &default_responses_options(),
    ))
    .expect("wire JSON");
    let input = wire["input"].as_array().unwrap();
    let search = input
        .iter()
        .find(|item| item["type"] == "tool_search_call")
        .expect("tool search call");
    let result = input
        .iter()
        .find(|item| item["type"] == "tool_search_output")
        .expect("tool search output");

    assert_eq!(wire["tools"][0]["name"], "base_tool");
    assert_eq!(search["execution"], "client");
    assert_eq!(search["status"], "completed");
    assert_eq!(result["call_id"], search["call_id"]);
    assert_eq!(result["tools"][0]["name"], "late_tool");
    assert_eq!(result["tools"][0]["defer_loading"], true);
}

fn decoded_message() -> AssistantMessage {
    let events = decode_openai_responses_sse(
        br#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.4"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}

data: {"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}

data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"content":[],"encrypted_content":"cipher","status":"completed"}}

data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[],"status":"in_progress","phase":"final_answer"}}

data: {"type":"response.output_text.delta","output_index":1,"delta":"answer"}

data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}],"status":"completed","phase":"final_answer"}}

data: {"type":"response.output_item.added","output_index":2,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read_file","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"README.md\"}"}

data: {"type":"response.output_item.done","output_index":2,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}","namespace":"dynamic_tools","status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.4","status":"completed","usage":{"input_tokens":20,"output_tokens":9,"total_tokens":29,"input_tokens_details":{"cached_tokens":5,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":4}}}}

"#,
        decode_context(),
    );
    let persisted = serde_json::to_vec(terminal(&events)).expect("persist assistant message");
    serde_json::from_slice(&persisted).expect("restore assistant message")
}

fn turn_two_wire() -> Vec<u8> {
    let model = responses_model("openai", "openai-responses", "gpt-5.4");
    let typed = typed_responses(&model);
    let compat =
        OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat).unwrap();
    let mut context = user_context(None);
    let message = decoded_message();
    let call_id = message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolCall { call, .. } => Some(call.id.clone()),
            _ => None,
        })
        .expect("tool call");
    context.messages.push(Message::Assistant(message));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("result-1"),
            tool_call_id: call_id,
            tool_name: "read_file".into(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("result-text"),
                text: "contents".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::from_unix_millis(2),
        }));
    encode_responses(&typed, &compat, &context, &default_responses_options())
}

fn default_responses_options() -> OpenAiResponsesOptions {
    OpenAiResponsesOptions {
        max_output_tokens: Some(128_000),
        temperature: None,
        sampling: OrderedJsonObject::new(),
        reasoning_effort: None,
        reasoning_summary: None,
        service_tier: None,
        tool_choice: None,
        cache_retention: CacheRetention::None,
        session_id: None,
    }
}

fn assert_captured_responses_family(family: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = root.join("../fixtures").join(family);
    let mut cases = fs::read_dir(&root)
        .expect("Responses fixture family")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    cases.sort();
    assert_eq!(cases.len(), 28, "captured {family} fixture count changed");
    for case in cases {
        assert_captured_responses_case(family, &case);
    }
}

fn assert_captured_responses_case(family: &str, case: &Path) {
    let canonical: Value = serde_json::from_slice(
        &fs::read(case.join("canonical.json")).expect("canonical Responses fixture"),
    )
    .expect("canonical Responses JSON");
    let model = captured_responses_model(&canonical["model"], family);
    let mut context = captured_responses_context(&canonical["context"], &model);
    let actual_turn_one = encode_captured_responses(&model, &context, &canonical);
    let expected_turn_one =
        fs::read(case.join("request-turn-1.body.json")).expect("captured turn one");
    assert_eq!(
        actual_turn_one,
        expected_turn_one,
        "turn-one Responses body mismatch for {family}/{}",
        case.file_name().unwrap().to_string_lossy()
    );

    let compat = captured_responses_compat(&canonical["model"]["compat"]);
    let grammar_tool_input_properties =
        responses_grammar_tool_input_properties(&context, &compat).expect("fixture grammar tools");
    let response = fs::read(case.join("response-turn-1.sse")).expect("captured response SSE");
    let assistant = decode_openai_responses_sse(
        &response,
        OpenAiResponsesDecodeContext {
            message_id: MessageId::new("fixture-turn-one-assistant"),
            provider: model.common.model_ref.provider.clone(),
            api: family.into(),
            requested_model: model.common.model_ref.model.clone(),
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
            grammar_tool_input_properties,
            pricing: model.common.pricing.clone(),
            requested_service_tier: None,
        },
    )
    .last()
    .and_then(AssistantEvent::terminal_message)
    .expect("terminal fixture assistant")
    .clone();
    let persisted = serde_json::to_vec(&assistant).expect("persist fixture assistant");
    let assistant: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore fixture assistant");
    context.messages.push(Message::Assistant(assistant));
    let first_index = context.messages.len();
    for (offset, message) in canonical["turnTwoAppend"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        context.messages.push(captured_responses_message(
            message,
            first_index + offset,
            &model,
        ));
    }

    let actual_turn_two = encode_captured_responses(&model, &context, &canonical);
    let expected_turn_two =
        fs::read(case.join("request-turn-2.body.json")).expect("captured turn two");
    assert_eq!(
        actual_turn_two,
        expected_turn_two,
        "turn-two Responses body mismatch for {family}/{}",
        case.file_name().unwrap().to_string_lossy()
    );
}

fn captured_responses_model(value: &Value, family: &str) -> ModelDescriptor {
    let mut model_headers = HeaderMapSpec::new();
    for (name, value) in value["headers"].as_object().into_iter().flatten() {
        model_headers.insert(name.clone(), value.as_str().map(str::to_owned));
    }
    let config = OpenAiResponsesModelConfig {
        compat: captured_responses_compat(&value["compat"]),
        thinking_levels: captured_thinking_levels(&value["thinkingLevelMap"]),
        sampling_defaults: captured_ordered_object(&value["samplingParams"]),
    };
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: pi_ai::ModelRef::new(
                value["provider"].as_str().expect("fixture provider"),
                value["id"].as_str().expect("fixture model"),
            ),
            display_name: value["name"].as_str().expect("fixture name").into(),
            base_url: Url::parse(if family == "openai-codex-responses" {
                "http://127.0.0.1:43123"
            } else {
                "http://127.0.0.1:43123/v1"
            })
            .unwrap(),
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
            headers: model_headers,
        },
        api: match family {
            "openai-codex-responses" => ApiModelConfig::OpenAiCodexResponses(config),
            "azure-openai-responses" => {
                let azure = AzureOpenAiResponsesModelConfig { responses: config };
                ApiModelConfig::Custom(CustomApiModelConfig {
                    api: ApiId::new("azure-openai-responses"),
                    schema_version: 1,
                    value: RawValue::from_string(serde_json::to_string(&azure).unwrap()).unwrap(),
                })
            }
            _ => ApiModelConfig::OpenAiResponses(config),
        },
        extensions: Default::default(),
    }
}

fn captured_responses_compat(value: &Value) -> OpenAiResponsesCompat {
    let optional_bool = |name: &str| value.get(name).and_then(Value::as_bool);
    OpenAiResponsesCompat {
        supports_developer_role: optional_bool("supportsDeveloperRole"),
        session_affinity_format: value
            .get("sessionAffinityFormat")
            .and_then(Value::as_str)
            .map(|format| match format {
                "openai" => SessionAffinityFormat::OpenAi,
                "openrouter" => SessionAffinityFormat::OpenRouter,
                "openai-nosession" => SessionAffinityFormat::OpenAiNoSession,
                other => panic!("unknown session affinity {other}"),
            }),
        supports_long_cache_retention: optional_bool("supportsLongCacheRetention"),
        supports_strict_mode: optional_bool("supportsStrictMode"),
        supports_openai_grammar_tools: optional_bool("supportsOpenAIGrammarTools"),
        supports_additional_tools: optional_bool("supportsAdditionalTools"),
        supports_tool_search: optional_bool("supportsToolSearch"),
        supports_explicit_prompt_cache_mode: optional_bool("supportsExplicitPromptCacheMode"),
        extensions: Default::default(),
    }
}

fn captured_thinking_levels(value: &Value) -> ThinkingLevelMap<OpenAiThinkingValue> {
    let level = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(|effort| LevelSupport::Value(OpenAiThinkingValue::Effort(effort.into())))
    };
    ThinkingLevelMap {
        off: level("off"),
        minimal: level("minimal"),
        low: level("low"),
        medium: level("medium"),
        high: level("high"),
        xhigh: level("xhigh"),
        max: level("max"),
    }
}

fn captured_responses_context(value: &Value, model: &ModelDescriptor) -> Context {
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
            .map(|(index, message)| captured_responses_message(message, index, model))
            .collect(),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(captured_responses_tool)
            .collect(),
    }
}

fn captured_responses_message(value: &Value, index: usize, model: &ModelDescriptor) -> Message {
    let id = MessageId::new(format!("fixture-message-{index}"));
    let timestamp =
        Timestamp::from_unix_millis(value["timestamp"].as_i64().unwrap_or(1_700_000_000_000));
    match value["role"].as_str().expect("fixture role") {
        "user" => Message::User(UserMessage {
            id,
            content: captured_responses_content(&value["content"], index),
            timestamp,
        }),
        "assistant" => Message::Assistant(AssistantMessage {
            id,
            provider: value["provider"]
                .as_str()
                .unwrap_or(model.common.model_ref.provider.as_str())
                .into(),
            api: value["api"]
                .as_str()
                .unwrap_or(model.api.api_id().as_str())
                .into(),
            requested_model: value["model"]
                .as_str()
                .unwrap_or(model.common.model_ref.model.as_str())
                .into(),
            response_model: None,
            response_id: None,
            end_turn: None,
            diagnostics: Vec::new(),
            content: captured_responses_content(&value["content"], 10_000 + index),
            replay: ReplayEnvelope::new(ReplayScope::new(
                value["provider"]
                    .as_str()
                    .unwrap_or(model.common.model_ref.provider.as_str()),
                value["api"].as_str().unwrap_or(model.api.api_id().as_str()),
                value["model"]
                    .as_str()
                    .unwrap_or(model.common.model_ref.model.as_str()),
                value["model"]
                    .as_str()
                    .unwrap_or(model.common.model_ref.model.as_str()),
            )),
            usage: captured_usage(&value["usage"]),
            cost: None,
            finish: AssistantFinish {
                reason: match value["stopReason"].as_str().unwrap_or("stop") {
                    "stop" => AssistantFinishReason::Stop,
                    "length" => AssistantFinishReason::Length,
                    "toolUse" => AssistantFinishReason::ToolUse,
                    "error" => AssistantFinishReason::Error,
                    "aborted" => AssistantFinishReason::Aborted,
                    other => panic!("unknown fixture stop reason {other}"),
                },
                raw_provider_reason: None,
                error: None,
            },
            timestamp,
        }),
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
            usage: None,
            added_tool_names: Vec::new(),
            is_error: value["isError"].as_bool().unwrap_or(false),
            timestamp,
        }),
        other => panic!("unknown fixture role {other}"),
    }
}

fn captured_responses_content(value: &Value, message_index: usize) -> Vec<ContentBlock> {
    if let Some(text) = value.as_str() {
        return vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("fixture-block-{message_index}-0")),
            text: text.into(),
        }];
    }
    value
        .as_array()
        .expect("fixture content")
        .iter()
        .enumerate()
        .map(|(block_index, block)| {
            let id = ContentBlockId::new(format!("fixture-block-{message_index}-{block_index}"));
            match block["type"].as_str().expect("fixture content type") {
                "text" => ContentBlock::Text {
                    id,
                    text: block["text"].as_str().expect("fixture text").into(),
                },
                "image" => ContentBlock::Image {
                    id,
                    data: block["data"].as_str().expect("fixture image").into(),
                    mime_type: block["mimeType"]
                        .as_str()
                        .expect("fixture image MIME")
                        .into(),
                },
                "thinking" => ContentBlock::Thinking {
                    id,
                    text: block["thinking"].as_str().expect("fixture thinking").into(),
                    redacted: false,
                    replay_item: None,
                },
                "toolCall" => ContentBlock::ToolCall {
                    id,
                    call: ToolCall {
                        id: ToolCallId::new(block["id"].as_str().expect("fixture tool call")),
                        name: block["name"].as_str().expect("fixture tool name").into(),
                        arguments: block["arguments"].clone(),
                    },
                },
                other => panic!("unknown fixture content type {other}"),
            }
        })
        .collect()
}

fn captured_responses_tool(value: &Value) -> ToolSpec {
    let constrained_sampling = value.get("constrainedSampling").map(|value| {
        let strict = match value["strict"].as_str().expect("fixture strict mode") {
            "require" => JsonSchemaStrictMode::Require,
            "prefer" => JsonSchemaStrictMode::Prefer,
            other => panic!("unknown fixture strict mode {other}"),
        };
        ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })
    });
    ToolSpec {
        schema_version: 1,
        name: value["name"].as_str().expect("fixture tool name").into(),
        description: value["description"]
            .as_str()
            .expect("fixture tool description")
            .into(),
        parameters: value["parameters"].clone(),
        constrained_sampling,
    }
}

fn captured_usage(value: &Value) -> Usage {
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

fn encode_captured_responses(
    model: &ModelDescriptor,
    context: &Context,
    canonical: &Value,
) -> Vec<u8> {
    let projected =
        transform_context_for_model(context, model, &Default::default(), &OpenAiResponsesHandoff)
            .expect("fixture Responses handoff")
            .context;
    let simple = canonical["entrypoint"] == "streamSimple";
    if canonical["family"] == "openai-codex-responses" {
        let typed = typed_codex(model);
        let compat =
            OpenAiCodexResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
                .expect("Codex fixture compat");
        let options = if simple {
            let simple = captured_simple_options(&canonical["options"]);
            let estimate = estimate_context_tokens(&projected).expect("fixture estimate");
            OpenAiCodexResponses::lower_simple(
                SimpleLoweringContext {
                    model: &typed,
                    compat: &compat,
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
                &Default::default(),
            )
            .expect("lower Codex fixture")
        } else {
            captured_codex_full_options(&canonical["options"])
        };
        let wire = OpenAiCodexResponses::encode(
            EncodeContext {
                model: &typed,
                context: &projected,
                compat: &compat,
                effective_base_url: &model.common.base_url,
            },
            &options,
        )
        .expect("encode Codex fixture");
        OrderedJsonWriter::to_vec(&wire.into()).expect("Codex fixture wire")
    } else if canonical["family"] == "azure-openai-responses" {
        let typed = typed_azure(model);
        let config = pi_ai_openai::azure_model_config(&typed.config).expect("Azure fixture config");
        let compat =
            AzureOpenAiResponses::resolve_compat(&model.common.base_url, &config.responses.compat)
                .expect("Azure fixture compat");
        let values = &canonical["options"];
        let options = if simple {
            let simple = captured_simple_options(values);
            let estimate = estimate_context_tokens(&projected).expect("fixture estimate");
            AzureOpenAiResponses::lower_simple(
                SimpleLoweringContext {
                    model: &typed,
                    compat: &compat,
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
                &pi_ai_openai::AzureOpenAiResponsesSimplePatch {
                    reasoning_summary: None,
                    azure_base_url: values
                        .get("azureBaseUrl")
                        .and_then(Value::as_str)
                        .map(|value| Url::parse(value).unwrap()),
                    azure_resource_name: values
                        .get("azureResourceName")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    azure_deployment_name: values
                        .get("azureDeploymentName")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    azure_api_version: values
                        .get("azureApiVersion")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            )
            .expect("lower Azure fixture")
        } else {
            AzureOpenAiResponsesOptions {
                responses: captured_public_full_options(values),
                azure_base_url: values
                    .get("azureBaseUrl")
                    .and_then(Value::as_str)
                    .map(|value| Url::parse(value).unwrap()),
                azure_resource_name: values
                    .get("azureResourceName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                deployment_name: values
                    .get("azureDeploymentName")
                    .and_then(Value::as_str)
                    .unwrap_or(model.common.model_ref.model.as_str())
                    .to_owned(),
                api_version: values
                    .get("azureApiVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("v1")
                    .to_owned(),
            }
        };
        let wire = AzureOpenAiResponses::encode(
            EncodeContext {
                model: &typed,
                context: &projected,
                compat: &compat,
                effective_base_url: &model.common.base_url,
            },
            &options,
        )
        .expect("encode Azure fixture");
        OrderedJsonWriter::to_vec(&wire.into()).expect("Azure fixture wire")
    } else {
        let typed = typed_responses(model);
        let compat = OpenAiResponses::resolve_compat(&model.common.base_url, &typed.config.compat)
            .expect("Responses fixture compat");
        let options = if simple {
            let simple = captured_simple_options(&canonical["options"]);
            let estimate = estimate_context_tokens(&projected).expect("fixture estimate");
            OpenAiResponses::lower_simple(
                SimpleLoweringContext {
                    model: &typed,
                    compat: &compat,
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
                &Default::default(),
            )
            .expect("lower Responses fixture")
        } else {
            captured_public_full_options(&canonical["options"])
        };
        encode_responses(&typed, &compat, &projected, &options)
    }
}

fn captured_public_full_options(value: &Value) -> OpenAiResponsesOptions {
    OpenAiResponsesOptions {
        max_output_tokens: value
            .get("maxTokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        sampling: captured_ordered_object(&value["samplingParams"]),
        reasoning_effort: value
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reasoning_summary: value.get("reasoningSummary").map(|summary| {
            summary.as_str().map(|summary| {
                serde_json::from_value(Value::String(summary.into())).expect("reasoning summary")
            })
        }),
        service_tier: value
            .get("serviceTier")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tool_choice: value.get("toolChoice").cloned().map(OrderedJsonValue::from),
        cache_retention: captured_cache_retention(value),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn captured_codex_full_options(value: &Value) -> OpenAiCodexResponsesOptions {
    OpenAiCodexResponsesOptions {
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        reasoning_effort: value
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reasoning_summary: value.get("reasoningSummary").map(|summary| {
            summary.as_str().map(|summary| {
                serde_json::from_value(Value::String(summary.into())).expect("Codex summary")
            })
        }),
        service_tier: value
            .get("serviceTier")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text_verbosity: value
            .get("textVerbosity")
            .and_then(Value::as_str)
            .map(|verbosity| {
                serde_json::from_value(Value::String(verbosity.into())).expect("text verbosity")
            })
            .unwrap_or_default(),
        tool_choice: match value.get("toolChoice").and_then(Value::as_str) {
            None | Some("auto") => OpenAiCodexToolChoice::Auto,
            Some("none") => OpenAiCodexToolChoice::None,
            Some("required") => OpenAiCodexToolChoice::Required,
            Some(other) => panic!("unknown Codex tool choice {other}"),
        },
        cache_retention: captured_cache_retention(value),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn captured_simple_options(value: &Value) -> SimpleGenerationOptions {
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
                serde_json::from_value::<ReasoningLevel>(Value::String(reasoning.into()))
                    .expect("fixture reasoning")
            }),
        sampling: captured_ordered_object(&value["samplingParams"]),
        cache_retention: value
            .get("cacheRetention")
            .and_then(Value::as_str)
            .map(|_| captured_cache_retention(value)),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tool_choice: match value.get("toolChoice").and_then(Value::as_str) {
            Some("auto") => Some(ToolChoice::Auto),
            Some("none") => Some(ToolChoice::None),
            Some("required") | None => None,
            Some(other) => panic!("unknown simple tool choice {other}"),
        },
        headers,
        max_retries: value
            .get("maxRetries")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        timeout_ms: value.get("timeoutMs").and_then(Value::as_u64),
        ..Default::default()
    }
}

fn captured_cache_retention(value: &Value) -> CacheRetention {
    match value
        .get("cacheRetention")
        .and_then(Value::as_str)
        .unwrap_or("short")
    {
        "none" => CacheRetention::None,
        "short" => CacheRetention::Short,
        "long" => CacheRetention::Long,
        other => panic!("unknown fixture retention {other}"),
    }
}

fn captured_ordered_object(value: &Value) -> OrderedJsonObject {
    if value.is_null() {
        return OrderedJsonObject::new();
    }
    match pi_ai::parse_ordered_json(serde_json::to_vec(value).unwrap())
        .expect("fixture ordered object")
    {
        OrderedJsonValue::Object(object) => object,
        _ => panic!("fixture sampling params are not an object"),
    }
}

fn responses_model(provider: &str, api: &str, model: &str) -> ModelDescriptor {
    let config = OpenAiResponsesModelConfig {
        compat: OpenAiResponsesCompat {
            supports_strict_mode: Some(true),
            ..Default::default()
        },
        thinking_levels: Default::default(),
        sampling_defaults: Default::default(),
    };
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: pi_ai::ModelRef::new(provider, model),
            display_name: model.into(),
            base_url: Url::parse(if api == "openai-codex-responses" {
                "https://chatgpt.com/backend-api"
            } else {
                "https://api.openai.com/v1"
            })
            .unwrap(),
            modalities: ModalityCapabilities {
                input: BTreeSet::from([Modality::Text, Modality::Image]),
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: 400_000,
                max_output_tokens: 128_000,
            },
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: true,
            headers: HeaderMapSpec::new(),
        },
        api: if api == "openai-codex-responses" {
            ApiModelConfig::OpenAiCodexResponses(config)
        } else {
            ApiModelConfig::OpenAiResponses(config)
        },
        extensions: Default::default(),
    }
}

fn typed_responses(model: &ModelDescriptor) -> TypedModelDescriptor<OpenAiResponses> {
    let ApiModelConfig::OpenAiResponses(config) = &model.api else {
        unreachable!()
    };
    TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    }
}

fn typed_codex(model: &ModelDescriptor) -> TypedModelDescriptor<OpenAiCodexResponses> {
    let ApiModelConfig::OpenAiCodexResponses(config) = &model.api else {
        unreachable!()
    };
    TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    }
}

fn typed_azure(model: &ModelDescriptor) -> TypedModelDescriptor<AzureOpenAiResponses> {
    let ApiModelConfig::Custom(config) = &model.api else {
        unreachable!()
    };
    TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    }
}

fn encode_responses(
    model: &TypedModelDescriptor<OpenAiResponses>,
    compat: &OpenAiResponsesCompat,
    context: &Context,
    options: &OpenAiResponsesOptions,
) -> Vec<u8> {
    let wire = OpenAiResponses::encode(
        EncodeContext {
            model,
            context,
            compat,
            effective_base_url: &model.common.base_url,
        },
        options,
    )
    .expect("encode Responses");
    OrderedJsonWriter::to_vec(&wire.into()).expect("wire")
}

fn user_context(system_prompt: Option<&str>) -> Context {
    let mut context = Context::new(system_prompt.map(str::to_owned));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("user-1"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("user-text"),
            text: "hello".into(),
        }],
        timestamp: Timestamp::from_unix_millis(1),
    }));
    context
}

fn fixture_text_context() -> Context {
    let mut context = user_context(None);
    let Message::User(message) = &mut context.messages[0] else {
        unreachable!()
    };
    let ContentBlock::Text { text, .. } = &mut message.content[0] else {
        unreachable!()
    };
    *text = "Return a concise fixture response.".into();
    context
}

fn captured_request(family: &str, case: &str, turn: u8) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures")
            .join(family)
            .join(case)
            .join(format!("request-turn-{turn}.body.json")),
    )
    .expect("captured Pi request body")
}

fn deferred_tools_context() -> Context {
    let mut context = user_context(None);
    context.tools = vec![tool_spec("base_tool"), tool_spec("late_tool")];
    context
        .messages
        .push(Message::Assistant(assistant_tool_message(
            "openai",
            "openai-responses",
            "gpt-5.4",
            "call_loader|fc_loader",
        )));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("load-result"),
            tool_call_id: ToolCallId::new("call_loader|fc_loader"),
            tool_name: "read_file".into(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("load-result-text"),
                text: "loaded".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: vec!["late_tool".into()],
            is_error: false,
            timestamp: Timestamp::from_unix_millis(2),
        }));
    context
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        schema_version: 1,
        name: name.into(),
        description: format!("{name} description"),
        parameters: serde_json::json!({"type":"object","properties":{}}),
        constrained_sampling: None,
    }
}

fn assistant_tool_message(provider: &str, api: &str, model: &str, id: &str) -> AssistantMessage {
    AssistantMessage {
        id: MessageId::new("assistant-tool"),
        provider: provider.into(),
        api: api.into(),
        requested_model: model.into(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::ToolCall {
            id: ContentBlockId::new("tool-block"),
            call: ToolCall {
                id: ToolCallId::new(id),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"README.md"}),
            },
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(provider, api, model, model)),
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::ToolUse,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(1),
    }
}

fn decode_context() -> OpenAiResponsesDecodeContext {
    OpenAiResponsesDecodeContext {
        message_id: MessageId::new("responses-message"),
        provider: "openai".into(),
        api: "openai-responses".into(),
        requested_model: "gpt-5.4".into(),
        timestamp: Timestamp::from_unix_millis(1),
        grammar_tool_input_properties: Default::default(),
        pricing: ModelPricing {
            default: TokenPriceRates::default(),
            request_wide_tiers: Vec::new(),
            cache_write_retention: CacheWriteRetentionPricing::default(),
        },
        requested_service_tier: None,
    }
}

fn priced_decode_context(api: &str, model: &str) -> OpenAiResponsesDecodeContext {
    let mut context = decode_context();
    context.api = api.into();
    context.requested_model = model.into();
    context.pricing.default.input = pi_ai::MoneyRate::new(1_000_000);
    context.pricing.default.output = pi_ai::MoneyRate::new(2_000_000);
    context
}

fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("terminal message")
}

fn replay<'a>(message: &'a AssistantMessage, kind: &str) -> &'a ReplayItem {
    message
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == kind)
        .expect("replay item")
}

fn replay_json(message: &AssistantMessage, kind: &str) -> serde_json::Value {
    serde_json::from_slice(replay(message, kind).json_bytes().expect("JSON bytes")).expect("JSON")
}
