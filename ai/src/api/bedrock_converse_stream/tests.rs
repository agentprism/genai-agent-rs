use super::*;
use crate::types::{
    AssistantRole, BedrockCompat, ImageContent, JsonObject, ModelCost, ModelInput,
    ThinkingContentType, ToolCallType, ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
use futures::StreamExt;

fn model(id: &str, name: &str, reasoning: bool) -> Model {
    let compat = BedrockCompat {
        supports_strict_mode: Some(true),
        ..BedrockCompat::default()
    };
    Model {
        id: id.to_owned(),
        name: name.to_owned(),
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_owned(),
        reasoning,
        thinking_level_map: None,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost::default(),
        context_window: 200_000.0,
        max_tokens: 64_000.0,
        sampling_params: None,
        headers: None,
        compat: Some(ModelCompat::Bedrock(compat)),
    }
}

fn claude() -> Model {
    model(
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "Claude Sonnet 4.5 (US)",
        true,
    )
}

fn nova() -> Model {
    let mut model = model("amazon.nova-lite-v1:0", "Nova Lite", false);
    model.compat = None;
    model
}

fn user(content: UserContent) -> Message {
    Message::User(Box::new(UserMessage {
        role: UserRole::User,
        content,
        timestamp: 1.0,
    }))
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

fn payload(context: &Context, model: &Model, options: &BedrockOptions) -> JsonValue {
    build_command_input(context, model, options, CacheRetention::None).expect("payload")
}

/// Ports pi `test/bedrock-convert-messages.test.ts:152-346`.
#[test]
fn blank_content_and_empty_assistant_messages_follow_bedrock_rules() {
    let model = claude();
    let options = BedrockOptions::default();
    let value = payload(
        &context(vec![
            user(UserContent::Text(("   ".to_owned()).into())),
            user(UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent::new("")),
                UserContentBlock::Text(TextContent::new("hello")),
            ])),
            Message::Assistant(Box::new(AssistantMessage {
                role: AssistantRole::Assistant,
                content: vec![AssistantContent::Text(TextContent::new("  "))],
                api: "bedrock-converse-stream".into(),
                provider: "amazon-bedrock".into(),
                model: model.id.clone().into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Default::default(),
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 1.0,
            })),
        ]),
        &model,
        &options,
    );
    assert_eq!(
        value["messages"],
        json!([
            {"role":"user","content":[{"text":"<empty>"}]},
            {"role":"user","content":[{"text":"hello"}]}
        ])
    );
}

/// Ports pi `test/bedrock-convert-messages.test.ts:271-305` and pins the
/// adjacent system/thinking lowering at `src/api/bedrock-converse-stream.ts:866-898,1004-1027`.
#[test]
fn lone_surrogates_are_removed_before_bedrock_blank_rules() {
    let model = claude();
    let lone = JsString::from_utf16(vec![0xd83d]);

    let mut user_context = context(vec![user(UserContent::Text(lone.clone()))]);
    user_context.system_prompt = Some(lone.clone());
    let value = payload(&user_context, &model, &BedrockOptions::default());
    assert_eq!(value["system"], json!([{"text":""}]));
    assert_eq!(
        value["messages"],
        json!([{"role":"user","content":[{"text":"<empty>"}]}])
    );

    let mut assistant =
        AssistantMessage::pending("bedrock-converse-stream", "amazon-bedrock", &model.id, 1.0);
    assistant.stop_reason = StopReason::Stop;
    assistant
        .content
        .push(AssistantContent::Text(TextContent::new(lone.clone())));
    assistant
        .content
        .push(AssistantContent::Thinking(ThinkingContent::new(lone)));
    let value = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &model,
        &BedrockOptions::default(),
    );
    assert_eq!(value["messages"], json!([]));
}

/// Pins pi `types.ts:350-467` together with
/// `src/api/bedrock-converse-stream.ts:866-898`: persistence deserialization
/// must retain lone surrogates until Bedrock's explicit sanitization boundary.
#[test]
fn deserialized_lone_surrogates_reach_bedrock_sanitization() {
    let context: Context = serde_json::from_str(
        r#"{"systemPrompt":"\ud83d","messages":[{"role":"user","content":"\ud83d","timestamp":0},{"role":"assistant","content":[{"type":"text","text":"\ud83d"},{"type":"thinking","thinking":"\ude00"}],"api":"bedrock-converse-stream","provider":"amazon-bedrock","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":1}]}"#,
    )
    .expect("context with ECMAScript strings");
    let value = payload(&context, &claude(), &BedrockOptions::default());
    assert_eq!(value["system"], json!([{"text":""}]));
    assert_eq!(
        value["messages"],
        json!([{"role":"user","content":[{"text":"<empty>"}]}])
    );
}

/// Ports pi `test/bedrock-convert-messages.test.ts:348-409`.
#[test]
fn replay_sanitizes_empty_tool_input_keys_without_mutating_source() {
    let model = claude();
    let arguments = json!({"keep":1,"":{"nested":true},"array":[{"":2,"ok":3}]});
    let mut assistant = AssistantMessage::pending(
        "bedrock-converse-stream",
        "amazon-bedrock",
        "different-model",
        1.0,
    );
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content.push(AssistantContent::ToolCall(ToolCall {
        kind: ToolCallType::ToolCall,
        id: "tool.id/with spaces".into(),
        name: "lookup".into(),
        arguments: JsonObject::try_from(arguments.clone()).expect("object arguments"),
        thought_signature: None,
        namespace: None,
    }));
    let value = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &model,
        &BedrockOptions::default(),
    );
    assert_eq!(
        value["messages"][0]["content"][0],
        json!({"toolUse":{"toolUseId":"tool_id_with_spaces","name":"lookup","input":{
            "keep":1,"array":[{"ok":3}]
        }}})
    );
    assert_eq!(arguments[""], json!({"nested":true}));
}

/// Pins pi `types.ts:372-380` and
/// `src/api/bedrock-converse-stream.ts:900-910`: recursive key filtering does
/// not discard neighboring dynamic values when JSON lowers non-finite leaves.
#[test]
fn bedrock_replay_preserves_dynamic_tool_arguments_around_nonfinite_leaves() {
    let model = claude();
    let mut arguments = JsonObject::new();
    arguments.insert("before", -1e20_f64);
    arguments.insert("nan", f64::NAN);
    arguments.insert(
        "nested",
        JsonValue::Array(vec![true.into(), f64::INFINITY.into(), "after".into()]),
    );
    let mut assistant =
        AssistantMessage::pending("bedrock-converse-stream", "amazon-bedrock", &model.id, 1.0);
    assistant.stop_reason = StopReason::ToolUse;
    assistant
        .content
        .push(AssistantContent::ToolCall(ToolCall::new(
            "call", "lookup", arguments,
        )));
    let value = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &model,
        &BedrockOptions::default(),
    );
    let input = &value["messages"][0]["content"][0]["toolUse"]["input"];
    assert_eq!(input["before"].as_f64(), Some(-1e20_f64));
    assert!(input["nan"].as_f64().is_some_and(f64::is_nan));
    assert_eq!(input["nested"][1].as_f64(), Some(f64::INFINITY));
    assert_eq!(
        crate::utils::ecma_json::stringify_provider_json(input),
        r#"{"before":-100000000000000000000,"nan":null,"nested":[true,null,"after"]}"#
    );
}

/// Pins pi `src/api/bedrock-converse-stream.ts:866-898`: only empty object
/// keys are removed; all other ECMAScript string keys and primitive values
/// reach the JSON wire unchanged.
#[test]
fn bedrock_replay_preserves_lone_surrogate_argument_keys_and_values() {
    let mut arguments = JsonObject::new();
    arguments.insert(
        JsString::from_utf16(vec![0xd83d]),
        JsonValue::String(JsString::from_utf16(vec![0xde00])),
    );
    arguments.insert(
        "plain",
        JsonValue::String(JsString::from_utf16(vec![0xd83d])),
    );
    arguments.insert("", "removed");
    let mut assistant = AssistantMessage::pending(
        "bedrock-converse-stream",
        "amazon-bedrock",
        &claude().id,
        1.0,
    );
    assistant.stop_reason = StopReason::ToolUse;
    assistant
        .content
        .push(AssistantContent::ToolCall(ToolCall::new(
            "call", "lookup", arguments,
        )));
    let request = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &claude(),
        &BedrockOptions::default(),
    );
    let wire = crate::utils::ecma_json::stringify_provider_json(&request);
    assert!(wire.contains(r#""input":{"\ud83d":"\ude00","plain":"\ud83d"}"#));
    assert!(!wire.contains(r#""":"removed""#));
}

/// Pins JavaScript `/[^a-zA-Z0-9_-]/g` plus `.slice(0, 64)` in pi
/// `src/api/bedrock-converse-stream.ts:876-879`: both operations count UTF-16
/// code units, including each half of an astral scalar.
#[test]
fn bedrock_tool_id_normalization_counts_utf16_code_units() {
    let source = JsString::from("a😀b");
    let assistant =
        AssistantMessage::pending("foreign-api", "foreign-provider", "foreign-model", 1.0);
    assert_eq!(
        normalize_tool_call_id(&source, &claude(), &assistant),
        "a__b"
    );

    let source = JsString::from(format!("{}😀z", "a".repeat(63)));
    let normalized = normalize_tool_call_id(&source, &claude(), &assistant);
    assert_eq!(normalized.len(), 64);
    assert_eq!(normalized.as_utf16()[63], u16::from(b'_'));
}

/// Pins the provider payload object built at pi
/// `src/api/bedrock-converse-stream.ts:989-1027`: opaque strings remain exact
/// ECMAScript strings until the JSON wire escapes isolated code units.
#[test]
fn bedrock_tool_ids_names_and_thinking_signatures_bypass_serde_json_value() {
    let model = claude();
    let mut thinking = ThinkingContent::new("reasoning");
    thinking.thinking_signature = Some(JsString::from_utf16(vec![0xd83d]));
    let call = ToolCall::new(
        JsString::from_utf16(vec![0xde00]),
        JsString::from_utf16(vec![0xd83d]),
        JsonObject::new(),
    );
    let mut assistant =
        AssistantMessage::pending("bedrock-converse-stream", "amazon-bedrock", &model.id, 1.0);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![
        AssistantContent::Thinking(thinking),
        AssistantContent::ToolCall(call),
    ];

    let request = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &model,
        &BedrockOptions::default(),
    );
    assert_eq!(
        request["messages"][0]["content"][0]["reasoningContent"]["reasoningText"]["signature"]
            .as_str()
            .expect("signature")
            .as_utf16(),
        &[0xd83d]
    );
    assert_eq!(
        request["messages"][0]["content"][1]["toolUse"]["toolUseId"]
            .as_str()
            .expect("tool id")
            .as_utf16(),
        &[0xde00]
    );
    assert_eq!(
        request["messages"][0]["content"][1]["toolUse"]["name"]
            .as_str()
            .expect("tool name")
            .as_utf16(),
        &[0xd83d]
    );

    let wire = crate::utils::ecma_json::stringify_provider_json(&request);
    assert!(wire.contains(r#""signature":"\ud83d""#));
    assert!(wire.contains(r#""toolUseId":"\ude00""#));
    assert!(wire.contains(r#""name":"\ud83d""#));
}

/// Ports pi `src/api/bedrock-converse-stream.ts:923-1079` where pi had no mutation-specific test.
#[test]
fn consecutive_tool_results_are_combined_and_blank_results_use_placeholder() {
    let results = vec![
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "a".into(),
            tool_name: "first".into(),
            content: vec![UserContentBlock::Text(TextContent::new(""))],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 1.0,
        })),
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "b".into(),
            tool_name: "second".into(),
            content: vec![UserContentBlock::Text(TextContent::new("failed"))],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: true,
            timestamp: 1.0,
        })),
    ];
    let value = payload(&context(results), &claude(), &BedrockOptions::default());
    assert_eq!(value["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(
        value["messages"][0]["content"],
        json!([
            {"toolResult":{"toolUseId":"a","content":[{"text":"<empty>"}],"status":"success"}},
            {"toolResult":{"toolUseId":"b","content":[{"text":"failed"}],"status":"error"}}
        ])
    );
}

/// Ports pi `test/bedrock-convert-messages.test.ts:63-103`.
#[test]
fn strict_tool_configuration_is_capability_gated() {
    use crate::types::{ConstrainedSamplingConfig, StrictPreference, ToolConstrainedSampling};
    let tool = Tool {
        name: "lookup".to_owned(),
        description: "Look up a value".to_owned(),
        parameters: json!({"type":"object","properties":{"value":{"type":"string"}}}),
        constrained_sampling: Some(ToolConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Prefer,
            },
        )),
    };
    let strict = convert_tool_config(Some(std::slice::from_ref(&tool)), None, true)
        .expect("valid")
        .expect("config");
    assert_eq!(strict["tools"][0]["toolSpec"]["strict"], true);
    let loose = convert_tool_config(Some(&[tool]), None, false)
        .expect("valid")
        .expect("config");
    assert!(loose["tools"][0]["toolSpec"].get("strict").is_none());
}

/// Ports pi `test/bedrock-thinking-payload.test.ts:35-164,198-265`.
#[test]
fn thinking_payloads_cover_fixed_adaptive_govcloud_and_non_claude_models() {
    let fixed = claude();
    let mut options = BedrockOptions {
        reasoning: Some(ThinkingLevel::Medium),
        ..BedrockOptions::default()
    };
    assert_eq!(
        build_additional_model_request_fields(&fixed, &options),
        Some(json!({
            "thinking":{"type":"enabled","budget_tokens":8192,"display":"summarized"},
            "anthropic_beta":["interleaved-thinking-2025-05-14"]
        }))
    );
    options.thinking_budgets = Some(ThinkingBudgets {
        medium: Some(4_321.0),
        ..ThinkingBudgets::default()
    });
    options.interleaved_thinking = Some(false);
    options.thinking_display = Some(BedrockThinkingDisplay::Omitted);
    assert_eq!(
        build_additional_model_request_fields(&fixed, &options),
        Some(json!({"thinking":{"type":"enabled","budget_tokens":4321,"display":"omitted"}}))
    );

    let adaptive = model(
        "arn:aws:bedrock:us-east-1:123:application-inference-profile/example",
        "Claude Opus 4.8",
        true,
    );
    options.reasoning = Some(ThinkingLevel::Xhigh);
    options.thinking_budgets = None;
    assert_eq!(
        build_additional_model_request_fields(&adaptive, &options),
        Some(json!({
            "thinking":{"type":"adaptive","display":"omitted"},
            "output_config":{"effort":"xhigh"}
        }))
    );

    let mut gov = fixed.clone();
    gov.id = "arn:aws-us-gov:bedrock:us-gov-west-1:123:inference-profile/x".to_owned();
    options.reasoning = Some(ThinkingLevel::Low);
    assert_eq!(
        build_additional_model_request_fields(&gov, &options),
        Some(json!({
            "thinking":{"type":"enabled","budget_tokens":2048},
        }))
    );
    assert_eq!(
        build_additional_model_request_fields(&nova(), &options),
        None
    );

    for candidate in [
        ("anthropic.claude-fable-5", "Claude Fable 5"),
        ("anthropic.claude-sonnet-5", "Claude Sonnet 5"),
        ("anthropic.claude-opus-5", "Claude Opus 5"),
    ] {
        assert!(supports_adaptive_thinking(candidate.0, candidate.1));
    }

    let mut adaptive_gov = adaptive;
    options.region = Some("us-gov-west-1".to_owned());
    let fields = build_additional_model_request_fields(&adaptive_gov, &options).expect("fields");
    assert!(fields["thinking"].get("display").is_none());
    adaptive_gov.name = "unrelated".to_owned();
    assert!(build_additional_model_request_fields(&adaptive_gov, &options).is_none());
}

/// Ports pi `test/bedrock-thinking-payload.test.ts:118-164`.
#[test]
fn simple_options_add_fixed_thinking_tokens_but_not_adaptive_tokens() {
    let context = context(vec![user(UserContent::Text(("hello".to_owned()).into()))]);
    let mut simple = SimpleStreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        ..SimpleStreamOptions::default()
    };
    simple.stream.max_tokens = Some(2_000.0);
    let fixed = lower_simple_options(&claude(), &context, &simple);
    assert_eq!(fixed.stream.max_tokens, Some(10_192.0));
    assert_eq!(
        fixed
            .thinking_budgets
            .as_ref()
            .and_then(|budgets| budgets.medium),
        Some(8_192.0)
    );
    let adaptive_model = model("anthropic.claude-opus-4-8", "Claude Opus 4.8", true);
    let adaptive = lower_simple_options(&adaptive_model, &context, &simple);
    assert_eq!(adaptive.stream.max_tokens, Some(2_000.0));
    assert_eq!(adaptive.thinking_budgets, None);
}

/// Ports pi `test/bedrock-endpoint-resolution.test.ts` and `test/bedrock-credentials.test.ts`.
#[test]
fn endpoint_region_credential_and_auth_precedence_match_pi() {
    assert_eq!(
        standard_endpoint_region("https://bedrock-runtime-fips.eu-west-1.amazonaws.com.cn/path"),
        Some("eu-west-1".to_owned())
    );
    assert_eq!(standard_endpoint_region("https://example.test"), None);
    assert!(should_use_explicit_endpoint(
        "https://example.test",
        Some("us-west-2"),
        true
    ));
    assert!(!should_use_explicit_endpoint(
        "https://bedrock-runtime.us-east-1.amazonaws.com",
        Some("us-west-2"),
        false
    ));
    assert!(should_use_explicit_endpoint(
        "https://bedrock-runtime.eu-west-1.amazonaws.com",
        None,
        false
    ));

    let env = ProviderEnv::from([
        ("AWS_PROFILE".to_owned(), "stored".to_owned()),
        ("AWS_REGION".to_owned(), "eu-central-1".to_owned()),
        ("AWS_ACCESS_KEY_ID".to_owned(), "access".to_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_owned(), "secret".to_owned()),
        ("AWS_SESSION_TOKEN".to_owned(), "session".to_owned()),
        (
            "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
            "env-token".to_owned(),
        ),
        ("no_proxy".to_owned(), "*".to_owned()),
    ]);
    let options = BedrockOptions {
        stream: StreamOptions {
            request: crate::types::ProviderRequestOptions {
                env: Some(env),
                api_key: Some("api-token".to_owned()),
                ..Default::default()
            },
            ..StreamOptions::default()
        },
        bearer_token: Some("option-token".to_owned()),
        ..BedrockOptions::default()
    };
    let resolution = resolve_client_configuration(&claude(), &options).expect("resolution");
    assert_eq!(resolution.profile.as_deref(), Some("stored"));
    assert_eq!(resolution.region.as_deref(), Some("eu-central-1"));
    assert_eq!(resolution.bearer_token.as_deref(), Some("option-token"));
    assert_eq!(resolution.credentials, None);
    assert_eq!(resolution.endpoint, None);

    assert_eq!(
        arn_region("arn:aws:bedrock:us-west-2:123:application-inference-profile/x").as_deref(),
        Some("us-west-2")
    );
    assert_eq!(
        arn_region("arn:aws-us-gov:bedrock:us-gov-west-1:123:inference-profile/x").as_deref(),
        Some("us-gov-west-1")
    );
    assert_eq!(arn_region("arn:awsfoo:bedrock:us-west-2:123:x"), None);

    let credentials_env = ProviderEnv::from([
        ("AWS_ACCESS_KEY_ID".to_owned(), "access".to_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_owned(), "secret".to_owned()),
        ("AWS_SESSION_TOKEN".to_owned(), "session".to_owned()),
        ("no_proxy".to_owned(), "*".to_owned()),
    ]);
    let credentials = resolve_client_configuration(
        &claude(),
        &BedrockOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(credentials_env),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..BedrockOptions::default()
        },
    )
    .expect("credentials");
    assert_eq!(
        credentials.credentials,
        Some(StaticCredentials {
            access_key_id: "access".to_owned(),
            secret_access_key: "secret".to_owned(),
            session_token: Some("session".to_owned()),
        })
    );

    let generic_key = resolve_client_configuration(
        &claude(),
        &BedrockOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    api_key: Some("generic-key".to_owned()),
                    env: Some(ProviderEnv::from([("no_proxy".to_owned(), "*".to_owned())])),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..BedrockOptions::default()
        },
    )
    .expect("generic bearer");
    assert_eq!(generic_key.bearer_token.as_deref(), Some("generic-key"));

    let skip_auth = resolve_client_configuration(
        &claude(),
        &BedrockOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    api_key: Some("ignored".to_owned()),
                    env: Some(ProviderEnv::from([
                        ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
                        ("no_proxy".to_owned(), "*".to_owned()),
                    ])),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..BedrockOptions::default()
        },
    )
    .expect("skip auth");
    assert_eq!(skip_auth.bearer_token, None);
    assert_eq!(
        skip_auth.credentials.expect("dummy").access_key_id,
        "dummy-access-key"
    );
}

/// Pins pi `src/api/bedrock-converse-stream.ts:144-235`'s pre-try setup failure behavior.
#[tokio::test]
async fn invalid_proxy_setup_leaves_the_stream_unsettled() {
    let mut events = stream(
        &claude(),
        &context(vec![user(UserContent::Text(("hello".to_owned()).into()))]),
        BedrockOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(ProviderEnv::from([
                        (
                            "https_proxy".to_owned(),
                            "socks5://proxy.test:1080".to_owned(),
                        ),
                        ("no_proxy".to_owned(), "never-match.test".to_owned()),
                    ])),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..BedrockOptions::default()
        },
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), events.next())
            .await
            .is_err()
    );
}

/// Ports pi `test/bedrock-thinking-payload.test.ts:198-249`.
#[test]
fn model_name_enables_application_profile_cache_points() {
    let model = model(
        "arn:aws:bedrock:us-east-1:123:application-inference-profile/example",
        "Claude Sonnet 4.6",
        true,
    );
    let context = Context {
        system_prompt: Some(("system".to_owned()).into()),
        messages: vec![user(UserContent::Text(("hello".to_owned()).into()))],
        tools: None,
    };
    let value = build_command_input(
        &context,
        &model,
        &BedrockOptions::default(),
        CacheRetention::Short,
    )
    .expect("payload");
    assert_eq!(value["system"][1], json!({"cachePoint":{"type":"default"}}));
    assert_eq!(
        value["messages"][0]["content"][1],
        json!({"cachePoint":{"type":"default"}})
    );
}

/// Ports pi `test/bedrock-custom-headers.test.ts:66-201`.
#[test]
fn custom_header_filter_is_case_insensitive_and_preserves_other_headers() {
    for reserved in [
        "authorization",
        "Authorization",
        "HOST",
        "x-amz-date",
        "X-Amz-Custom",
    ] {
        assert!(is_reserved_header(reserved));
    }
    for allowed in ["x-custom", "content-type", "traceparent"] {
        assert!(!is_reserved_header(allowed));
    }

    let mut headers = aws_smithy_runtime_api::http::Headers::new();
    headers.insert("authorization", "real-auth");
    headers.insert("host", "real-host");
    headers.insert("x-amz-date", "real-date");
    sdk::apply_custom_headers(
        &mut headers,
        &IndexMap::from([
            ("Authorization".to_owned(), "evil".to_owned()),
            ("X-Amz-Date".to_owned(), "evil".to_owned()),
            ("HOST".to_owned(), "evil".to_owned()),
            ("x-custom".to_owned(), "ok".to_owned()),
        ]),
    )
    .expect("headers");
    assert_eq!(headers.get("authorization"), Some("real-auth"));
    assert_eq!(headers.get("host"), Some("real-host"));
    assert_eq!(headers.get("x-amz-date"), Some("real-date"));
    assert_eq!(headers.get("x-custom"), Some("ok"));

    let mut simple = SimpleStreamOptions::default();
    simple.stream.request.headers = Some(crate::types::ProviderHeaders::from([(
        "x-custom".to_owned(),
        Some("forwarded".to_owned()),
    )]));
    let lowered = lower_simple_options(
        &claude(),
        &context(vec![user(UserContent::Text(("hello".to_owned()).into()))]),
        &simple,
    );
    assert_eq!(
        lowered
            .stream
            .request
            .headers
            .as_ref()
            .and_then(|headers| headers.get("x-custom"))
            .and_then(Option::as_deref),
        Some("forwarded")
    );
}

/// Ports pi `test/bedrock-response-headers.test.ts:37-61` without opening a socket.
#[test]
fn raw_smithy_response_status_and_headers_are_forwarded_intact() {
    let response = http::Response::builder()
        .status(200)
        .header("x-amzn-requestid", "req-123")
        .header("x-bifrost-provider", "bedrock")
        .header("x-bifrost-resolved-model", "model-123")
        .body(aws_smithy_types::body::SdkBody::empty())
        .expect("response");
    let response = aws_smithy_runtime_api::client::orchestrator::HttpResponse::try_from(response)
        .expect("Smithy response");
    let response = sdk::to_provider_response(&response);
    assert_eq!(response.status, 200.0);
    assert_eq!(response.headers["x-amzn-requestid"], "req-123");
    assert_eq!(response.headers["x-bifrost-provider"], "bedrock");
    assert_eq!(response.headers["x-bifrost-resolved-model"], "model-123");
}

/// Ports pi `test/bedrock-error-metadata.test.ts` and the Bedrock cases in
/// `test/provider-error-body-regression.test.ts`.
#[test]
fn error_formatting_and_diagnostics_preserve_raw_body_and_modeled_metadata() {
    let error = BedrockError {
        message: "Unknown: UnknownError".to_owned(),
        details: Box::new(BedrockErrorDetails {
            name: Some("ValidationException".to_owned()),
            status: Some(403),
            body: Some("gateway denied".into()),
            message_carries_body: false,
            service_exception: true,
            request_id: Some(" request-123 ".to_owned()),
            fallback_request_id: None,
        }),
    };
    assert_eq!(
        format_bedrock_error(&error),
        "Validation error: 403: gateway denied"
    );
    let mut output = pending_message(&claude());
    append_bedrock_failure_diagnostic(&mut output, &error, Some("fallback"));
    assert_eq!(
        output.diagnostics.as_ref().expect("diagnostic")[0].details,
        Some(Map::from_iter([
            ("status".to_owned(), json!(403)),
            ("errorCode".to_owned(), json!("ValidationException")),
            ("requestId".to_owned(), json!("request-123")),
        ]))
    );
    let retention = BedrockError {
        message: "data retention mode 'default' is not available".to_owned(),
        details: BedrockError::plain("").details,
    };
    assert!(format_bedrock_error(&retention).contains(BEDROCK_DATA_RETENTION_DOCS_URL));

    let bare_stream = BedrockError::plain("Too many requests")
        .with_fallback_request_id(Some("stream-request".to_owned()));
    let mut output = pending_message(&claude());
    append_bedrock_failure_diagnostic(
        &mut output,
        &bare_stream,
        bare_stream.fallback_request_id.as_deref(),
    );
    assert_eq!(format_bedrock_error(&bare_stream), "Too many requests");
    assert_eq!(
        output.diagnostics.expect("diagnostic")[0].details,
        Some(Map::from_iter([(
            "requestId".to_owned(),
            json!("stream-request")
        )]))
    );
}

/// Ports the full Bedrock catch path at pi
/// `src/api/bedrock-converse-stream.ts:374-389,1301-1315` and
/// `src/utils/error-body.ts:76-81,137-139` with a real Smithy raw response.
#[test]
fn raw_bedrock_error_body_is_utf16_truncated_into_terminal_error_message() {
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::client::result::SdkError;
    use aws_smithy_types::body::SdkBody;
    use aws_smithy_types::error::metadata::ErrorMetadata;

    let body = format!("{}😀", "x".repeat(3_999));
    let response = http::Response::builder()
        .status(403)
        .body(SdkBody::from(body))
        .expect("raw response");
    let response = HttpResponse::try_from(response).expect("Smithy response");
    let sdk_error: SdkError<ErrorMetadata, HttpResponse> =
        SdkError::response_error(std::io::Error::other("unmodeled response"), response);
    let error = sdk::sdk_error(&sdk_error);
    let event = terminal_bedrock_error(pending_message(&claude()), &error, false);
    let AssistantMessageEvent::Error { error, .. } = event else {
        panic!("terminal Bedrock error")
    };
    let message = error.error_message.expect("errorMessage");
    let mut expected = "403: ".encode_utf16().collect::<Vec<_>>();
    expected.extend("x".repeat(3_999).encode_utf16());
    expected.push(0xd83d);
    expected.extend("... [truncated 1 chars]".encode_utf16());
    assert_eq!(message.as_utf16(), expected);
}

/// Ports pi `test/bedrock-raw-stop-reason.test.ts`.
#[test]
fn raw_and_normalized_stop_reasons_are_both_retained() {
    assert_eq!(map_stop_reason(Some("end_turn")), (StopReason::Stop, None));
    assert_eq!(
        map_stop_reason(Some("model_context_window_exceeded")),
        (StopReason::Length, None)
    );
    assert_eq!(
        map_stop_reason(Some("future_reason")),
        (
            StopReason::Error,
            Some("Provider stopped with: future_reason".to_owned())
        )
    );
}

/// Ports pi `test/bedrock-redacted-reasoning.test.ts` and the tool-argument stream case in
/// `test/bedrock-convert-messages.test.ts:105-150`.
#[tokio::test]
async fn stream_state_buffers_redacted_reasoning_and_preserves_empty_tool_argument_keys() {
    let model = claude();
    let (sender, mut events) = AssistantMessageEventStream::channel();
    let mut output = pending_message(&model);
    let mut state = StreamState::default();
    for event in [
        BedrockStreamEvent::MessageStart {
            role: "assistant".to_owned(),
        },
        BedrockStreamEvent::ReasoningDelta {
            provider_index: 0,
            text: None,
            signature: Some("discard-me".to_owned()),
            redacted_content: Some(vec![1, 2]),
        },
        BedrockStreamEvent::ReasoningDelta {
            provider_index: 0,
            text: None,
            signature: Some("ignored-after-redaction".to_owned()),
            redacted_content: Some(vec![3, 4]),
        },
        BedrockStreamEvent::ContentBlockStop { provider_index: 0 },
        BedrockStreamEvent::ContentBlockStart {
            provider_index: 1,
            tool_id: Some("tool-1".to_owned()),
            tool_name: Some("edit".to_owned()),
        },
        BedrockStreamEvent::ToolDelta {
            provider_index: 1,
            input: "{\"ok\":1,\"\":\"\"}".to_owned(),
        },
        BedrockStreamEvent::ContentBlockStop { provider_index: 1 },
        BedrockStreamEvent::MessageStop {
            reason: Some("tool_use".to_owned()),
        },
    ] {
        handle_stream_event(event, &mut state, &model, &mut output, &sender).expect("event");
    }
    drop(sender);
    let captured = events.by_ref().collect::<Vec<_>>().await;
    assert!(captured.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::ThinkingDelta { delta, .. }
            if delta == REDACTED_THINKING_PLACEHOLDER
    )));
    let AssistantContent::Thinking(thinking) = &output.content[0] else {
        panic!("thinking");
    };
    assert_eq!(thinking.kind, ThinkingContentType::Thinking);
    assert_eq!(thinking.thinking, REDACTED_THINKING_PLACEHOLDER);
    assert_eq!(thinking.redacted, Some(true));
    assert_eq!(thinking.thinking_signature.as_deref(), Some("AQIDBA=="));
    let AssistantContent::ToolCall(call) = &output.content[1] else {
        panic!("tool call");
    };
    assert_eq!(
        call.arguments,
        JsonObject::try_from(json!({"ok":1,"":""})).expect("object arguments")
    );
    assert_eq!(output.stop_reason, StopReason::ToolUse);
    assert_eq!(output.raw_stop_reason.as_deref(), Some("tool_use"));
}

/// Ports pi `test/bedrock-redacted-reasoning.test.ts:165-186`.
#[tokio::test]
async fn unstopped_redacted_reasoning_is_flushed_when_the_stream_succeeds() {
    let model = claude();
    let (sender, _events) = AssistantMessageEventStream::channel();
    let mut output = pending_message(&model);
    let mut state = StreamState::default();
    handle_stream_event(
        BedrockStreamEvent::ReasoningDelta {
            provider_index: 0,
            text: None,
            signature: None,
            redacted_content: Some(vec![1, 2, 3, 4]),
        },
        &mut state,
        &model,
        &mut output,
        &sender,
    )
    .expect("reasoning delta");
    handle_stream_event(
        BedrockStreamEvent::MessageStop {
            reason: Some("end_turn".to_owned()),
        },
        &mut state,
        &model,
        &mut output,
        &sender,
    )
    .expect("message stop");

    sdk::finish_stream_result(&mut state, &mut output, Ok(())).expect("stream success");

    assert!(state.blocks.is_empty());
    let AssistantContent::Thinking(thinking) = &output.content[0] else {
        panic!("thinking");
    };
    assert_eq!(thinking.redacted, Some(true));
    assert_eq!(thinking.thinking_signature.as_deref(), Some("AQIDBA=="));
    assert_eq!(output.stop_reason, StopReason::Stop);
}

/// Pins pi `src/api/bedrock-converse-stream.ts:328-331,668-683` for failures before block stop.
#[tokio::test]
async fn unstopped_redacted_reasoning_is_flushed_before_error_or_abort_propagates() {
    for failure in ["Throttling error", "Request was aborted"] {
        let model = claude();
        let (sender, _events) = AssistantMessageEventStream::channel();
        let mut output = pending_message(&model);
        let mut state = StreamState::default();
        handle_stream_event(
            BedrockStreamEvent::ReasoningDelta {
                provider_index: 0,
                text: None,
                signature: None,
                redacted_content: Some(vec![1, 2, 3, 4]),
            },
            &mut state,
            &model,
            &mut output,
            &sender,
        )
        .expect("reasoning delta");

        let error =
            sdk::finish_stream_result(&mut state, &mut output, Err(BedrockError::plain(failure)))
                .expect_err("stream failure");

        assert_eq!(error.message, failure);
        assert!(state.blocks.is_empty());
        let AssistantContent::Thinking(thinking) = &output.content[0] else {
            panic!("thinking");
        };
        assert_eq!(thinking.redacted, Some(true));
        assert_eq!(thinking.thinking_signature.as_deref(), Some("AQIDBA=="));
    }
}

/// Ports pi `test/bedrock-redacted-reasoning.test.ts:164-273` replay behavior.
#[test]
fn redacted_and_signed_reasoning_replay_matches_model_capabilities() {
    let model = claude();
    let mut assistant = AssistantMessage::pending(
        "bedrock-converse-stream",
        "amazon-bedrock",
        model.id.clone(),
        1.0,
    );
    let mut redacted = ThinkingContent::new(REDACTED_THINKING_PLACEHOLDER);
    redacted.redacted = Some(true);
    redacted.thinking_signature = Some("AQIDBA==".into());
    let mut signed = ThinkingContent::new("private chain");
    signed.thinking_signature = Some("signature".into());
    assistant.content = vec![
        AssistantContent::Thinking(redacted),
        AssistantContent::Thinking(signed),
    ];
    assistant.stop_reason = StopReason::Stop;
    let value = payload(
        &context(vec![Message::Assistant(Box::new(assistant.clone()))]),
        &model,
        &BedrockOptions::default(),
    );
    assert_eq!(
        value["messages"][0]["content"],
        json!([
            {"reasoningContent":{"redactedContent":"AQIDBA=="}},
            {"reasoningContent":{"reasoningText":{"text":"private chain","signature":"signature"}}}
        ])
    );

    let mut unsigned = ThinkingContent::new("visible fallback");
    unsigned.thinking_signature = Some(" ".into());
    assistant.content = vec![AssistantContent::Thinking(unsigned)];
    let value = payload(
        &context(vec![Message::Assistant(Box::new(assistant))]),
        &model,
        &BedrockOptions::default(),
    );
    assert_eq!(
        value["messages"][0]["content"],
        json!([{"text":"visible fallback"}])
    );
}

/// Pins pi `src/api/bedrock-converse-stream.ts:1266-1310`.
#[test]
fn image_formats_and_invalid_redacted_payloads_are_handled() {
    for (mime, format) in [
        ("image/jpeg", "jpeg"),
        ("image/jpg", "jpeg"),
        ("image/png", "png"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
    ] {
        assert_eq!(image_block(mime, "AQI=").expect("image")["format"], format);
    }
    assert_eq!(
        image_block("image/svg+xml", "AQI=")
            .expect_err("unsupported")
            .message,
        "Unknown image type: image/svg+xml"
    );
    let image = ImageContent::new("AQI=", "image/png");
    assert_eq!(
        convert_tool_result_content(&[UserContentBlock::Image(image)]).expect("content")[0]["image"]
            ["source"]["bytes"],
        "AQI="
    );
    assert_eq!(decode_browser_base64(" A Q I \n").expect("atob"), [1, 2]);
}

/// Pins pi `src/api/bedrock-converse-stream.ts:394-401,685-697,734-739`.
#[test]
fn javascript_string_and_usage_semantics_are_retained() {
    assert_eq!(
        model_match_candidates("id", "Claude\u{00a0}__Opus::4.8")[3],
        "claude-opus-4-8"
    );
    assert!(normalize_diagnostic_value(Some(&"😀".repeat(100))).is_some());
    assert!(normalize_diagnostic_value(Some(&"😀".repeat(101))).is_none());

    let model = claude();
    let (sender, _events) = AssistantMessageEventStream::channel();
    let mut output = pending_message(&model);
    handle_stream_event(
        BedrockStreamEvent::Metadata {
            input: Some(3),
            output: Some(4),
            cache_read: Some(0),
            cache_write: Some(0),
            total: Some(0),
        },
        &mut StreamState::default(),
        &model,
        &mut output,
        &sender,
    )
    .expect("metadata");
    assert_eq!(output.usage.total_tokens, 7.0);
}

/// Pins pi `src/api/bedrock-converse-stream.ts:626-697` against the real AWS SDK unions.
#[test]
fn real_sdk_reasoning_and_metadata_unions_map_one_member_at_a_time() {
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ConverseStreamMetadataEvent,
        ConverseStreamOutput, ReasoningContentBlockDelta, TokenUsage,
    };
    use aws_smithy_types::Blob;

    let reasoning = [
        (
            ReasoningContentBlockDelta::Text("thought".to_owned()),
            Some("thought"),
            None,
            None,
        ),
        (
            ReasoningContentBlockDelta::Signature("signature".to_owned()),
            None,
            Some("signature"),
            None,
        ),
        (
            ReasoningContentBlockDelta::RedactedContent(Blob::new([1, 2, 3])),
            None,
            None,
            Some(vec![1, 2, 3]),
        ),
    ];
    for (delta, expected_text, expected_signature, expected_redacted) in reasoning {
        let event = ContentBlockDeltaEvent::builder()
            .content_block_index(7)
            .delta(ContentBlockDelta::ReasoningContent(delta))
            .build()
            .expect("SDK reasoning event");
        let Some(BedrockStreamEvent::ReasoningDelta {
            provider_index,
            text,
            signature,
            redacted_content,
        }) = sdk::convert_sdk_event(ConverseStreamOutput::ContentBlockDelta(event))
        else {
            panic!("reasoning delta");
        };
        assert_eq!(provider_index, 7);
        assert_eq!(text.as_deref(), expected_text);
        assert_eq!(signature.as_deref(), expected_signature);
        assert_eq!(redacted_content, expected_redacted);
    }

    assert!(
        sdk::convert_sdk_event(ConverseStreamOutput::Metadata(
            ConverseStreamMetadataEvent::builder().build()
        ))
        .is_none()
    );
    let usage = TokenUsage::builder()
        .input_tokens(3)
        .output_tokens(4)
        .total_tokens(7)
        .cache_read_input_tokens(1)
        .cache_write_input_tokens(2)
        .build()
        .expect("SDK token usage");
    let Some(BedrockStreamEvent::Metadata {
        input,
        output,
        cache_read,
        cache_write,
        total,
    }) = sdk::convert_sdk_event(ConverseStreamOutput::Metadata(
        ConverseStreamMetadataEvent::builder().usage(usage).build(),
    ))
    else {
        panic!("metadata");
    };
    assert_eq!(
        (input, output, cache_read, cache_write, total),
        (Some(3), Some(4), Some(1), Some(2), Some(7),)
    );
}

/// Ports pi `test/bedrock-error-metadata.test.ts:132-152` using the error
/// variants returned by the real AWS SDK event receiver.
#[test]
fn real_sdk_stream_errors_preserve_modeled_vs_unmodeled_metadata() {
    use aws_sdk_bedrockruntime::types::error::{
        ConverseStreamOutputError, InternalServerException, ModelStreamErrorException,
        ServiceUnavailableException, ThrottlingException, ValidationException,
    };
    use aws_smithy_runtime_api::client::result::SdkError;
    use aws_smithy_types::error::metadata::ErrorMetadata;
    use aws_smithy_types::event_stream::RawMessage;

    let errors = [
        ConverseStreamOutputError::InternalServerException(
            InternalServerException::builder()
                .message("failure")
                .build(),
        ),
        ConverseStreamOutputError::ModelStreamErrorException(
            ModelStreamErrorException::builder()
                .message("failure")
                .build(),
        ),
        ConverseStreamOutputError::ValidationException(
            ValidationException::builder().message("failure").build(),
        ),
        ConverseStreamOutputError::ThrottlingException(
            ThrottlingException::builder().message("failure").build(),
        ),
        ConverseStreamOutputError::ServiceUnavailableException(
            ServiceUnavailableException::builder()
                .message("failure")
                .build(),
        ),
    ];
    for service in errors {
        let error = SdkError::service_error(service, RawMessage::invalid(None));
        let error = sdk::event_stream_error(&error, Some("request-123".to_owned()));
        assert_eq!(format_bedrock_error(&error), "failure");
        let mut output = pending_message(&claude());
        append_bedrock_failure_diagnostic(
            &mut output,
            &error,
            error.fallback_request_id.as_deref(),
        );
        assert_eq!(
            output.diagnostics.expect("diagnostic")[0].details,
            Some(Map::from_iter([(
                "requestId".to_owned(),
                json!("request-123")
            )]))
        );
    }

    let unmodeled = ConverseStreamOutputError::generic(
        ErrorMetadata::builder()
            .code("ModelStreamErrorException")
            .message("Model stream terminated unexpectedly.")
            .build(),
    );
    let error = SdkError::service_error(unmodeled, RawMessage::invalid(None));
    let error = sdk::event_stream_error(&error, Some("request-123".to_owned()));
    assert_eq!(
        format_bedrock_error(&error),
        "Model stream terminated unexpectedly."
    );
    let mut output = pending_message(&claude());
    append_bedrock_failure_diagnostic(&mut output, &error, error.fallback_request_id.as_deref());
    assert_eq!(
        output.diagnostics.expect("diagnostic")[0].details,
        Some(Map::from_iter([
            ("errorCode".to_owned(), json!("ModelStreamErrorException")),
            ("requestId".to_owned(), json!("request-123")),
        ]))
    );
}
