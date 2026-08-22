use super::*;
use crate::types::{
    Api, AssistantMessage, AssistantRole, Context, ImageContent, JsonObject, JsonValue, Message,
    ModelCost, ProviderId, StopReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage,
    ToolResultRole, UserContent, UserMessage, UserRole,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: Api::from("google-generative-ai"),
        provider: ProviderId::from("google"),
        base_url: "https://example.test/v1beta".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost::default(),
        context_window: 128_000.0,
        max_tokens: 8_192.0,
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

fn assistant(model: &Model, content: Vec<AssistantContent>) -> Message {
    let mut message = AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        2.0,
    );
    message.role = AssistantRole::Assistant;
    message.content = content;
    message.stop_reason = StopReason::ToolUse;
    Message::Assistant(Box::new(message))
}

fn tool_result(id: &str, content: Vec<UserContentBlock>) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content,
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 3.0,
    }))
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

fn google_error(status: u16) -> GoogleSdkError {
    GoogleSdkError::new(adk_gemini::ClientError::BadResponse {
        code: status,
        description: Some(format!("got status: {status}")),
    })
}

/// Ports pi `test/google-shared-retry.test.ts:9-40`.
#[tokio::test(start_paused = true)]
async fn google_sdk_status_errors_follow_the_shared_retry_policy() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let request_attempts = Arc::clone(&attempts);
    let retrying = tokio::spawn(async move {
        retry_google_request(
            move || {
                let attempt = request_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(google_error(429))
                    } else {
                        Ok("ok")
                    }
                }
            },
            crate::utils::provider_retry::ProviderRetryOptions {
                max_retries: Some(1.0),
                ..Default::default()
            },
        )
        .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(retrying.await.expect("task").expect("retry"), "ok");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let attempts = Arc::new(AtomicUsize::new(0));
    let request_attempts = Arc::clone(&attempts);
    retry_google_request(
        move || {
            request_attempts.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(google_error(429)) }
        },
        Default::default(),
    )
    .await
    .expect_err("unset max retries");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let attempts = Arc::new(AtomicUsize::new(0));
    let request_attempts = Arc::clone(&attempts);
    retry_google_request(
        move || {
            request_attempts.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(google_error(400)) }
        },
        crate::utils::provider_retry::ProviderRetryOptions {
            max_retries: Some(2.0),
            ..Default::default()
        },
    )
    .await
    .expect_err("400 must not retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Ports pi `test/google-thinking-signature.test.ts:4-37`.
#[test]
fn thinking_detection_and_signature_retention_match_pi() {
    assert!(is_thinking_part(&json!({ "thought": true })));
    assert!(is_thinking_part(
        &json!({ "thought": true, "thoughtSignature": "opaque" })
    ));
    assert!(!is_thinking_part(&json!({ "thoughtSignature": "opaque" })));
    assert!(!is_thinking_part(
        &json!({ "thought": false, "thoughtSignature": "opaque" })
    ));
    assert_eq!(
        retain_thought_signature(None, Some("sig-1")).as_deref(),
        Some("sig-1")
    );
    assert_eq!(
        retain_thought_signature(Some("sig-1"), None).as_deref(),
        Some("sig-1")
    );
    assert_eq!(
        retain_thought_signature(Some("sig-1"), Some("")).as_deref(),
        Some("sig-1")
    );
    assert_eq!(
        retain_thought_signature(Some("sig-1"), Some("sig-2")).as_deref(),
        Some("sig-2")
    );
}

/// Ports pi `test/google-shared-convert-tools.test.ts:10-203`.
#[test]
fn tool_conversion_strips_only_openapi_meta_keys_and_enables_validated_mode() {
    let original = json!({
        "$schema": "draft-07",
        "$id": "root",
        "$defs": { "keptBecauseArrayRuleIsSeparate": { "type": "string" } },
        "type": "object",
        "properties": {
            "deep": { "$schema": "draft-07", "$id": "nested", "type": "string" },
            "reference": { "$ref": "#/$defs/value", "type": "string" }
        },
        "required": ["deep"]
    });
    let tool = Tool {
        name: "test_tool".to_owned(),
        description: "A test tool".to_owned(),
        parameters: original.clone(),
        constrained_sampling: None,
    };
    let converted = convert_tools(std::slice::from_ref(&tool), true, true)
        .expect("conversion")
        .expect("tools");
    let parameters = &converted[0]["functionDeclarations"][0]["parameters"];
    assert_eq!(
        parameters,
        &json!({
            "type": "object",
            "properties": {
                "deep": { "type": "string" },
                "reference": { "$ref": "#/$defs/value", "type": "string" }
            },
            "required": ["deep"]
        })
    );
    assert_eq!(tool.parameters, original);
    let json_schema = convert_tools(std::slice::from_ref(&tool), false, true)
        .expect("conversion")
        .expect("tools");
    assert_eq!(
        json_schema[0]["functionDeclarations"][0]["parametersJsonSchema"],
        original
    );
    assert_eq!(convert_tools(&[], false, true).expect("empty"), None);
    assert!(supports_google_strict_tool_sampling(
        "gemini-3.1-pro-preview"
    ));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));

    let mut strict = Tool {
        parameters: json!({ "type": "object", "properties": {} }),
        ..tool
    };
    strict.constrained_sampling = Some(crate::types::ToolConstrainedSampling::Config(
        crate::types::ConstrainedSamplingConfig::JsonSchema {
            strict: crate::types::StrictPreference::Require,
        },
    ));
    assert_eq!(
        resolve_google_function_calling_mode(&[strict.clone()], None, true).expect("strict mode"),
        Some(GoogleFunctionCallingMode::Validated)
    );
    assert!(
        resolve_google_function_calling_mode(&[strict], None, false)
            .expect_err("unsupported")
            .contains("requires JSON-schema constrained sampling")
    );
}

/// Ports pi `test/google-shared-signed-empty-blocks.test.ts:48-117`.
#[test]
fn signed_empty_blocks_are_kept_only_for_the_same_model() {
    const SIGNATURE: &str = "AAAAAAAAAAAAAAAAAAAAAA==";
    let target = model("gemini-3-pro-preview");
    let mut thinking = ThinkingContent::new("");
    thinking.thinking_signature = Some(SIGNATURE.into());
    let mut text = TextContent::new("");
    text.text_signature = Some(SIGNATURE.into());
    let converted = convert_messages(
        &target,
        &context(vec![
            user("Hi"),
            assistant(
                &target,
                vec![
                    AssistantContent::Thinking(thinking.clone()),
                    AssistantContent::Text(text.clone()),
                    AssistantContent::ToolCall(ToolCall::new(
                        "call_1",
                        "bash",
                        JsonObject::try_from(json!({ "command": "ls" })).expect("object arguments"),
                    )),
                ],
            ),
        ]),
    );
    let model_turn = converted
        .iter()
        .find(|content| content["role"] == "model")
        .expect("model turn");
    assert_eq!(
        model_turn["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .filter(|part| part["thoughtSignature"] == SIGNATURE)
            .count(),
        2
    );

    let mut foreign = target.clone();
    foreign.id = "other-model".to_owned();
    let converted = convert_messages(
        &target,
        &context(vec![
            user("Hi"),
            assistant(
                &foreign,
                vec![
                    AssistantContent::Thinking(thinking),
                    AssistantContent::Text(text),
                    AssistantContent::ToolCall(ToolCall::new(
                        "call_1",
                        "bash",
                        JsonObject::try_from(json!({ "command": "ls" })).expect("object arguments"),
                    )),
                ],
            ),
        ]),
    );
    let model_turn = converted
        .iter()
        .find(|content| content["role"] == "model")
        .expect("model turn");
    assert_eq!(model_turn["parts"].as_array().expect("parts").len(), 1);
    assert!(!model_turn.to_string().contains(SIGNATURE));
}

/// Ports pi `test/google-shared-gemini3-unsigned-tool-call.test.ts:80-167`.
#[test]
fn gemini_three_preserves_ids_without_fabricating_signatures() {
    let target = model("gemini-3.6-flash");
    let converted = convert_messages(
        &target,
        &context(vec![
            user("Hi"),
            assistant(
                &target,
                vec![
                    AssistantContent::ToolCall(ToolCall::new("call_1", "bash", JsonObject::new())),
                    AssistantContent::ToolCall(ToolCall::new("call_2", "bash", JsonObject::new())),
                ],
            ),
            tool_result(
                "call_1",
                vec![UserContentBlock::Text(TextContent::new("one"))],
            ),
            tool_result(
                "call_2",
                vec![UserContentBlock::Text(TextContent::new("two"))],
            ),
        ]),
    );
    let parts = converted
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert_eq!(
        parts
            .iter()
            .filter_map(|part| part["functionCall"]["id"].as_str())
            .collect::<Vec<_>>(),
        ["call_1", "call_2"]
    );
    assert_eq!(
        parts
            .iter()
            .filter_map(|part| part["functionResponse"]["id"].as_str())
            .collect::<Vec<_>>(),
        ["call_1", "call_2"]
    );
    assert!(!converted.iter().any(|value| {
        value
            .to_string()
            .contains("skip_thought_signature_validator")
    }));
    assert!(requires_tool_call_id("claude-sonnet-4-5"));
    assert!(requires_tool_call_id("gpt-oss-120b"));
    assert!(!requires_tool_call_id("gemini-2.5-flash"));
}

/// Ports pi `test/google-shared-gemini3-unsigned-tool-call.test.ts:129-155`.
#[test]
fn tool_signatures_are_kept_only_when_valid_and_replayable() {
    const SIGNATURE: &str = "AAAAAAAAAAAAAAAAAAAAAA==";
    let target = model("gemini-3.6-flash");
    let mut signed = ToolCall::new("call_1", "bash", JsonObject::new());
    signed.thought_signature = Some(SIGNATURE.into());
    let converted = convert_messages(
        &target,
        &context(vec![assistant(
            &target,
            vec![AssistantContent::ToolCall(signed.clone())],
        )]),
    );
    assert_eq!(converted[0]["parts"][0]["thoughtSignature"], SIGNATURE);

    let mut foreign = target.clone();
    foreign.id = "other-model".to_owned();
    let converted = convert_messages(
        &target,
        &context(vec![assistant(
            &foreign,
            vec![AssistantContent::ToolCall(signed)],
        )]),
    );
    assert!(converted[0]["parts"][0].get("thoughtSignature").is_none());

    let legacy = model("gemini-2.5-flash");
    let converted = convert_messages(
        &legacy,
        &context(vec![
            assistant(
                &foreign,
                vec![AssistantContent::ToolCall(ToolCall::new(
                    "call_1",
                    "bash",
                    JsonObject::new(),
                ))],
            ),
            tool_result(
                "call_1",
                vec![UserContentBlock::Text(TextContent::new("done"))],
            ),
        ]),
    );
    assert!(converted[0]["parts"][0]["functionCall"].get("id").is_none());
    assert!(
        converted[1]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
}

/// Pins pi `types.ts:372-380` and `src/api/google-shared.ts:203-214`:
/// provider replay never replaces a valid dynamic argument object merely
/// because one nested JavaScript number is non-finite.
#[test]
fn google_replay_preserves_dynamic_tool_arguments_around_nonfinite_leaves() {
    let target = model("gemini-3.6-flash");
    let mut arguments = JsonObject::new();
    arguments.insert("before", -1e20_f64);
    arguments.insert("nan", f64::NAN);
    arguments.insert(
        "nested",
        JsonValue::Array(vec![1.into(), f64::NEG_INFINITY.into(), "after".into()]),
    );
    let converted = convert_messages(
        &target,
        &context(vec![assistant(
            &target,
            vec![AssistantContent::ToolCall(ToolCall::new(
                "call", "lookup", arguments,
            ))],
        )]),
    );
    let args = &converted[0]["parts"][0]["functionCall"]["args"];
    assert_eq!(args["before"].as_f64(), Some(-1e20_f64));
    assert!(args["nan"].as_f64().is_some_and(f64::is_nan));
    assert_eq!(args["nested"][1].as_f64(), Some(f64::NEG_INFINITY));
    assert_eq!(
        crate::utils::ecma_json::stringify_provider_json(args),
        r#"{"before":-100000000000000000000,"nan":null,"nested":[1,null,"after"]}"#
    );
}

/// Ports pi `test/google-shared-image-tool-result-routing.test.ts:68-102`.
#[test]
fn image_tool_results_use_separate_turn_before_gemini_three() {
    let mut target = model("gemini-2.5-flash");
    let history = |model: &Model| {
        context(vec![
            user("read files"),
            assistant(
                model,
                vec![
                    AssistantContent::ToolCall(ToolCall::new("call_a", "read", JsonObject::new())),
                    AssistantContent::ToolCall(ToolCall::new(
                        "call_img",
                        "read",
                        JsonObject::new(),
                    )),
                    AssistantContent::ToolCall(ToolCall::new("call_b", "read", JsonObject::new())),
                ],
            ),
            tool_result(
                "call_a",
                vec![UserContentBlock::Text(TextContent::new("alpha"))],
            ),
            tool_result(
                "call_img",
                vec![UserContentBlock::Image(ImageContent::new(
                    "abc",
                    "image/png",
                ))],
            ),
            tool_result(
                "call_b",
                vec![UserContentBlock::Text(TextContent::new("beta"))],
            ),
        ])
    };
    let converted = convert_messages(&target, &history(&target));
    assert_eq!(converted.len(), 5);
    assert_eq!(converted[3]["parts"][0]["text"], "Tool result image:");
    assert!(converted[3]["parts"][1].get("inlineData").is_some());

    target.id = "gemini-3-pro-preview".to_owned();
    let converted = convert_messages(&target, &history(&target));
    assert_eq!(converted.len(), 3);
    assert_eq!(converted[2]["parts"].as_array().expect("parts").len(), 3);
    assert!(
        converted[2]["parts"][1]["functionResponse"]["parts"][0]
            .get("inlineData")
            .is_some()
    );
}

/// Ports pi `test/google-thinking-level-map.test.ts:100-128`.
#[test]
fn thinking_level_resolution_is_exhaustive_and_validates_mappings() {
    let mut target = model("gemini-3.7-flash");
    assert_eq!(
        resolve_google_thinking_level(&target, ModelThinkingLevel::Off),
        Ok(ResolvedGoogleThinkingLevel::High)
    );
    assert_eq!(
        resolve_google_thinking_level(&target, ModelThinkingLevel::Minimal),
        Ok(ResolvedGoogleThinkingLevel::Minimal)
    );
    target.thinking_level_map = Some(crate::types::ThinkingLevelMap {
        high: Some(Some("LOW".to_owned())),
        xhigh: Some(Some("HIGH".to_owned())),
        ..Default::default()
    });
    assert_eq!(
        resolve_google_thinking_level(&target, ModelThinkingLevel::High),
        Ok(ResolvedGoogleThinkingLevel::Low)
    );
    assert_eq!(
        resolve_google_thinking_level(&target, ModelThinkingLevel::Xhigh),
        Ok(ResolvedGoogleThinkingLevel::High)
    );
    target.thinking_level_map.as_mut().expect("map").xhigh = Some(Some("extreme".to_owned()));
    assert_eq!(
        resolve_google_thinking_level(&target, ModelThinkingLevel::Xhigh)
            .expect_err("invalid mapping"),
        "Unsupported Google thinking level mapping for google/gemini-3.7-flash: xhigh -> extreme"
    );
}
