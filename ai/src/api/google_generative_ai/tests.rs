use super::*;
use crate::types::{
    Api, AssistantContent, AssistantMessage, Context, FetchFunction, JsString, JsonObject,
    JsonValue, Message, ModelCost, ModelInput, ProviderHttpRequest, ProviderHttpResponse,
    ProviderId, StopReason, ToolCall, UserContent, UserMessage, UserRole,
};
use futures::StreamExt;
use reqwest_012::header::{HeaderMap, USER_AGENT};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: Api::from("google-generative-ai"),
        provider: ProviderId::from("test-google"),
        base_url: "https://example.invalid/v1beta".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000.0,
        max_tokens: 4_096.0,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![crate::types::Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(("Hello".to_owned()).into()),
            timestamp: 0.0,
        }))],
        tools: None,
    }
}

/// Pins pi `src/api/google-shared.ts:203-214` at the request-body boundary:
/// tool argument maps are replayed intact and JSON uses ECMAScript key order.
#[test]
fn request_wire_replays_lossless_ordered_tool_arguments() {
    let target = model("gemini-3-pro-preview");
    let mut arguments = JsonObject::new();
    arguments.insert("10", "ten");
    arguments.insert("2", "two");
    arguments.insert(
        JsString::from_utf16(vec![0xd83d]),
        JsonValue::String(JsString::from_utf16(vec![0xde00])),
    );
    let mut replay = AssistantMessage::pending(
        target.api.clone(),
        target.provider.clone(),
        target.id.clone(),
        1.0,
    );
    replay.content = vec![AssistantContent::ToolCall(ToolCall::new(
        JsString::from_utf16(vec![0xd801]),
        JsString::from_utf16(vec![0xdc01]),
        arguments,
    ))];
    replay.stop_reason = StopReason::ToolUse;
    let request_context = Context {
        system_prompt: None,
        messages: vec![Message::Assistant(Box::new(replay))],
        tools: None,
    };
    let params =
        build_params(&target, &request_context, &GoogleOptions::default()).expect("request params");
    let wire = crate::utils::ecma_json::stringify_provider_json(&params);
    assert!(wire.contains(
        r#""functionCall":{"name":"\udc01","args":{"2":"two","10":"ten","\ud83d":"\ude00"},"id":"\ud801"}"#
    ));
}

struct UncalledFetch;

impl FetchFunction for UncalledFetch {
    fn fetch(
        &self,
        _request: ProviderHttpRequest,
    ) -> futures::future::BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        Box::pin(async { Err("custom fetch must not be called".to_owned()) })
    }
}

/// Ports the Google assertions in pi `test/google-thinking-disable.test.ts:74-156`.
#[test]
fn disabled_thinking_uses_each_model_familys_lowest_supported_control() {
    assert_eq!(
        disabled_thinking_config(&model("gemini-3.1-pro-preview")),
        json!({ "thinkingLevel": "LOW" })
    );
    assert_eq!(
        disabled_thinking_config(&model("gemini-3-flash-preview")),
        json!({ "thinkingLevel": "MINIMAL" })
    );
    assert_eq!(
        disabled_thinking_config(&model("gemma-4-27b")),
        json!({ "thinkingLevel": "MINIMAL" })
    );
    assert_eq!(
        disabled_thinking_config(&model("gemini-2.5-flash")),
        json!({ "thinkingBudget": 0 })
    );
}

/// Ports pi `src/api/google-generative-ai.ts:419-525` and the payload assertions in
/// `test/google-thinking-level-map.test.ts:130-164`.
#[test]
fn thinking_levels_and_budgets_match_google_model_families() {
    assert_eq!(
        thinking_level(
            ResolvedGoogleThinkingLevel::Minimal,
            &model("gemini-3.1-pro-preview")
        ),
        GoogleApiThinkingLevel::Low
    );
    assert_eq!(
        thinking_level(
            ResolvedGoogleThinkingLevel::Medium,
            &model("gemini-3.1-pro-preview")
        ),
        GoogleApiThinkingLevel::High
    );
    assert_eq!(
        thinking_level(ResolvedGoogleThinkingLevel::Low, &model("gemma4-27b")),
        GoogleApiThinkingLevel::Minimal
    );
    assert_eq!(
        google_budget(
            &model("gemini-2.5-pro"),
            ResolvedGoogleThinkingLevel::High,
            None
        ),
        32_768.0
    );
    assert_eq!(
        google_budget(
            &model("gemini-2.5-flash-lite"),
            ResolvedGoogleThinkingLevel::Minimal,
            None
        ),
        512.0
    );
    let custom = ThinkingBudgets {
        high: Some(1_234.0),
        ..Default::default()
    };
    assert_eq!(
        google_budget(
            &model("gemini-2.5-flash"),
            ResolvedGoogleThinkingLevel::High,
            Some(&custom)
        ),
        1_234.0
    );
}

/// Pins pi `src/api/google-generative-ai.ts:359-417` payload shape where no direct unit test
/// covers ordinary sampling, system instructions, and thinking together.
#[test]
fn build_params_preserves_pi_payload_shape() {
    let target = model("gemini-3.7-flash");
    let mut context = context();
    context.system_prompt = Some(("Be useful".to_owned()).into());
    let options = GoogleOptions {
        stream: StreamOptions {
            temperature: Some(0.25),
            max_tokens: Some(321.0),
            ..StreamOptions::default()
        },
        tool_choice: None,
        thinking: Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(GoogleApiThinkingLevel::High),
        }),
    };
    assert_eq!(
        build_params(&target, &context, &options).expect("params"),
        json!({
            "model": "gemini-3.7-flash",
            "contents": [{ "role": "user", "parts": [{ "text": "Hello" }] }],
            "config": {
                "temperature": 0.25,
                "maxOutputTokens": 321,
                "systemInstruction": "Be useful",
                "thinkingConfig": { "includeThoughts": true, "thinkingLevel": "HIGH" }
            }
        })
    );
}

/// Pins the replacement-payload path at pi `src/api/google-generative-ai.ts:88-93`.
#[test]
fn sdk_request_conversion_preserves_supported_payload_hook_fields() {
    let request = google_wire_request_from_params(
        &json!({
            "model": "gemini-3-flash-preview",
            "contents": [{ "role": "user", "parts": [{ "text": "Hello" }] }],
            "config": {
                "topP": 0.9,
                "stopSequences": ["END"],
                "responseMimeType": "application/json",
                "thinkingConfig": { "thinkingLevel": "HIGH" },
                "cachedContent": "cachedContents/example",
                "safetySettings": [],
                "systemInstruction": { "parts": [{ "text": "Be useful" }] },
                "tools": [],
                "toolConfig": { "functionCallingConfig": { "mode": "AUTO" } },
                "abortSignal": "client-only",
                "httpOptions": { "timeout": 1 }
            }
        }),
        &GoogleRequestTarget::mldev(),
    )
    .expect("request");
    let value = request.body;
    assert_eq!(value["generationConfig"]["topP"], 0.9);
    assert_eq!(value["generationConfig"]["stopSequences"], json!(["END"]));
    assert_eq!(
        value["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        value["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert_eq!(value["cachedContent"], "cachedContents/example");
    assert_eq!(value["systemInstruction"]["parts"][0]["text"], "Be useful");
    assert!(value.get("abortSignal").is_none());
    assert!(value.get("httpOptions").is_none());
}

/// Ports the finish-reason semantics in pi `test/google-raw-stop-reason.test.ts:112-180`.
#[test]
fn raw_finish_reasons_preserve_length_and_only_promote_stop_with_tools() {
    let target = model("gemini-3-flash-preview");
    let (sender, _stream) = AssistantMessageEventStream::channel();
    let mut output = crate::types::AssistantMessage::pending(
        target.api.clone(),
        target.provider.clone(),
        target.id.clone(),
        0.0,
    );
    let mut current = None;
    process_chunk(
        &sender,
        &target,
        &mut output,
        &json!({
            "candidates": [{
                "content": { "parts": [{ "functionCall": { "id": "call-1", "name": "echo", "args": {} } }] },
                "finishReason": "MAX_TOKENS"
            }]
        }),
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("chunk");
    assert_eq!(output.stop_reason, StopReason::Length);
    assert_eq!(output.raw_stop_reason.as_deref(), Some("MAX_TOKENS"));
    let AssistantContent::ToolCall(tool_call) = &output.content[0] else {
        panic!("tool call")
    };
    assert_eq!(tool_call.arguments, JsonObject::new());

    output.stop_reason = StopReason::Pending;
    process_chunk(
        &sender,
        &target,
        &mut output,
        &json!({ "candidates": [{ "finishReason": "STOP" }] }),
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("chunk");
    assert_eq!(output.stop_reason, StopReason::ToolUse);

    output.stop_reason = StopReason::Pending;
    process_chunk(
        &sender,
        &target,
        &mut output,
        &json!({ "candidates": [{ "finishReason": "MALFORMED_FUNCTION_CALL" }] }),
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("chunk");
    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.raw_stop_reason.as_deref(),
        Some("MALFORMED_FUNCTION_CALL")
    );

    assert_eq!(
        process_chunk(
            &sender,
            &target,
            &mut output,
            &json!({ "candidates": [{ "finishReason": "FUTURE_REASON" }] }),
            &mut current,
            &TOOL_CALL_COUNTER,
        ),
        Err("Unhandled stop reason: FUTURE_REASON".to_owned())
    );
}

/// Pins raw finish-reason preservation without SDK enum normalization at
/// pi `src/api/google-generative-ai.ts:203-207` and `src/api/google-vertex.ts:217-221`.
#[test]
fn raw_stream_decoder_does_not_coerce_finish_reasons() {
    for reason in [
        "IMAGE_PROHIBITED_CONTENT",
        "IMAGE_RECITATION",
        "IMAGE_OTHER",
        "NO_IMAGE",
        "FUTURE_REASON",
    ] {
        let event = json!({ "candidates": [{ "finishReason": reason }] }).to_string();
        let chunk = decode_google_stream_event(&event).expect("valid JSON response");
        assert_eq!(chunk["candidates"][0]["finishReason"], reason);
    }
}

/// Pins pi's observed-field response handling at `src/api/google-generative-ai.ts:104-202`:
/// missing tool arguments default to `{}` and unconsumed part shapes are ignored.
#[test]
fn raw_stream_decoder_accepts_missing_and_unmodeled_part_fields() {
    let target = model("gemini-3-flash-preview");
    let event = json!({
        "candidates": [{
            "content": {
                "parts": [
                    { "functionCall": { "name": "get_time" } },
                    {},
                    { "thoughtSignature": "x" },
                    { "futurePart": { "value": true } }
                ]
            },
            "finishReason": "STOP"
        }]
    })
    .to_string();
    let chunk = decode_google_stream_event(&event).expect("pi-compatible JSON response");
    let (sender, _stream) = AssistantMessageEventStream::channel();
    let mut output = crate::types::AssistantMessage::pending(
        target.api.clone(),
        target.provider.clone(),
        target.id.clone(),
        0.0,
    );
    let mut current = None;

    process_chunk(
        &sender,
        &target,
        &mut output,
        &chunk,
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("unmodeled parts are ignored");

    assert_eq!(output.content.len(), 1);
    let AssistantContent::ToolCall(tool_call) = &output.content[0] else {
        panic!("tool call")
    };
    assert_eq!(tool_call.name, "get_time");
    assert_eq!(tool_call.arguments, JsonObject::new());
    assert_eq!(output.stop_reason, StopReason::ToolUse);
}

/// Pins pi `src/api/google-generative-ai.ts:87-93`: client construction errors precede
/// payload construction and `onPayload`.
#[tokio::test]
async fn studio_client_configuration_errors_precede_payload_hooks() {
    let payload_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = payload_calls.clone();
    let mut options = GoogleOptions::default();
    options.stream.request.api_key = Some("test-key".to_owned());
    options.stream.request.on_payload = Some(Arc::new(move |_, _| {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Err("payload hook should not run".to_owned()) })
    }));
    let invalid_model = model("gemini-3-flash-preview");

    let message = stream(
        &Model {
            base_url: "not a valid URL".to_owned(),
            ..invalid_model
        },
        &context(),
        options,
    )
    .result()
    .await
    .expect("terminal result");

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(payload_calls.load(Ordering::Relaxed), 0);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| !error.contains("payload hook should not run"))
    );
}

/// Ports the Google adapter assertions in pi `test/fetch-option.test.ts:123-139`.
#[tokio::test]
async fn google_adapters_reject_custom_fetch_without_invoking_it() {
    let fetch: Arc<dyn FetchFunction> = Arc::new(UncalledFetch);
    let mut google_options = GoogleOptions::default();
    google_options.stream.request.api_key = Some("test-key".to_owned());
    google_options.stream.request.fetch = Some(fetch.clone());
    let google = stream(&model("gemini-3-flash-preview"), &context(), google_options)
        .result()
        .await
        .expect("terminal Google result");
    assert_eq!(google.stop_reason, StopReason::Error);
    assert!(google.error_message.as_deref().is_some_and(|message| {
        message.contains("Custom fetch is not supported by the Google Generative AI adapter")
    }));

    let mut vertex_options = crate::api::google_vertex::GoogleVertexOptions::default();
    vertex_options.stream.request.api_key = Some("test-key".to_owned());
    vertex_options.stream.request.fetch = Some(fetch);
    let vertex_model = Model {
        api: Api::from("google-vertex"),
        provider: ProviderId::from("google-vertex"),
        ..model("gemini-3-flash-preview")
    };
    let vertex = crate::api::google_vertex::stream(&vertex_model, &context(), vertex_options)
        .result()
        .await
        .expect("terminal Vertex result");
    assert_eq!(vertex.stop_reason, StopReason::Error);
    assert!(vertex.error_message.as_deref().is_some_and(|message| {
        message.contains("Custom fetch is not supported by the Google Vertex adapter")
    }));
}

/// Ports pi `test/fetch-option.test.ts:141-157` for the explicit ambient/default fetch.
#[tokio::test]
async fn google_adapters_accept_the_default_fetch_identity() {
    let mut google_options = GoogleOptions::default();
    google_options.stream.request.fetch = Some(crate::types::default_fetch());
    let google = stream(&model("gemini-3-flash-preview"), &context(), google_options)
        .result()
        .await
        .expect("terminal Google result");
    assert_eq!(google.stop_reason, StopReason::Error);
    assert!(
        google
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("No API key for provider: test-google"))
    );

    let mut vertex_options = crate::api::google_vertex::GoogleVertexOptions::default();
    vertex_options.stream.request.fetch = Some(crate::types::default_fetch());
    let vertex_model = Model {
        api: Api::from("google-vertex"),
        provider: ProviderId::from("google-vertex"),
        ..model("gemini-3-flash-preview")
    };
    let vertex = crate::api::google_vertex::stream(&vertex_model, &context(), vertex_options)
        .result()
        .await
        .expect("terminal Vertex result");
    assert_eq!(vertex.stop_reason, StopReason::Error);
    assert!(vertex.error_message.as_deref().is_some_and(|message| {
        message.contains("Vertex AI requires a project ID")
            && !message.contains("Custom fetch is not supported")
    }));
}

/// Pins pi `src/api/google-generative-ai.ts:100-102,140-163,224-242,246-260`.
#[tokio::test]
async fn streamed_metadata_and_signatures_preserve_the_first_response_id() {
    let target = model("gemini-3-flash-preview");
    let (sender, mut events) = AssistantMessageEventStream::channel();
    let mut output = crate::types::AssistantMessage::pending(
        target.api.clone(),
        target.provider.clone(),
        target.id.clone(),
        0.0,
    );
    let mut current = None;
    process_chunk(
        &sender,
        &target,
        &mut output,
        &json!({
            "responseId": "first-response",
            "candidates": [{ "content": { "parts": [{
                "text": "hello",
                "thoughtSignature": "signature"
            }] } }]
        }),
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("first chunk");
    process_chunk(
        &sender,
        &target,
        &mut output,
        &json!({
            "responseId": "later-response",
            "candidates": [{
                "content": { "parts": [{ "text": " world" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10.5,
                "cachedContentTokenCount": 2.25,
                "candidatesTokenCount": 3.5,
                "thoughtsTokenCount": 1.25,
                "totalTokenCount": 15.25
            }
        }),
        &mut current,
        &TOOL_CALL_COUNTER,
    )
    .expect("second chunk");
    close_current_block(&sender, &output, &mut current).expect("close text block");

    assert_eq!(output.response_id.as_deref(), Some("first-response"));
    let AssistantContent::Text(text) = &output.content[0] else {
        panic!("text block")
    };
    assert_eq!(text.text, "hello world");
    assert_eq!(text.text_signature.as_deref(), Some("signature"));
    assert_eq!(output.usage.input, 8.25);
    assert_eq!(output.usage.output, 4.75);
    assert_eq!(
        output
            .usage
            .reasoning
            .as_ref()
            .copied()
            .expect("reasoning usage"),
        1.25
    );
    assert_eq!(output.usage.total_tokens, 15.25);

    assert!(matches!(
        events.next().await,
        Some(AssistantMessageEvent::TextStart {
            content_index: 0.0,
            ..
        })
    ));
    assert!(
        matches!(events.next().await, Some(AssistantMessageEvent::TextDelta { delta, .. }) if delta == "hello")
    );
    assert!(
        matches!(events.next().await, Some(AssistantMessageEvent::TextDelta { delta, .. }) if delta == " world")
    );
    assert!(matches!(
        events.next().await,
        Some(AssistantMessageEvent::TextEnd { .. })
    ));
}

/// Pins the Google SDK-default fallback in pi `src/api/google-generative-ai.ts:343-356`.
#[test]
fn empty_model_base_url_uses_the_google_sdk_default_endpoint() {
    let mut target = model("gemini-3-flash-preview");
    target.base_url.clear();
    create_studio_backend(&target, "test-key", &GoogleOptions::default())
        .expect("default Google endpoint");
}

/// Ports pi `test/google-raw-stop-reason.test.ts:186-193` and the Vertex equivalent at
/// `test/google-vertex-api-key-resolution.test.ts:150-177`.
#[test]
fn explicit_headers_override_or_remove_the_default_google_user_agent() {
    let target = model("gemini-3-flash-preview");
    let headers = merged_google_headers(&target, HeaderMap::new(), &StreamOptions::default())
        .expect("default headers");
    assert_eq!(
        headers
            .get(USER_AGENT)
            .expect("User-Agent")
            .to_str()
            .expect("header text"),
        crate::utils::pi_user_agent::get_pi_user_agent()
    );

    let mut options = StreamOptions::default();
    options.request.headers = Some(crate::types::ProviderHeaders::from([(
        "User-Agent".to_owned(),
        Some("custom-agent".to_owned()),
    )]));
    let headers =
        merged_google_headers(&target, HeaderMap::new(), &options).expect("overridden headers");
    assert_eq!(headers.get(USER_AGENT).expect("User-Agent"), "custom-agent");

    options.request.headers = Some(crate::types::ProviderHeaders::from([(
        "User-Agent".to_owned(),
        None,
    )]));
    let headers =
        merged_google_headers(&target, HeaderMap::new(), &options).expect("removed headers");
    assert!(!headers.contains_key(USER_AGENT));

    let mut auth_headers = HeaderMap::new();
    auth_headers.insert("x-goog-api-key", "client-key".parse().expect("API key"));
    options.request.headers = Some(crate::types::ProviderHeaders::from([(
        "x-goog-api-key".to_owned(),
        None,
    )]));
    let headers = merged_google_client_headers(&target, auth_headers.clone(), &options)
        .expect("auth fills a removed key");
    assert_eq!(headers["x-goog-api-key"], "client-key");

    options.request.headers = Some(crate::types::ProviderHeaders::from([(
        "x-goog-api-key".to_owned(),
        Some("explicit-key".to_owned()),
    )]));
    let headers = merged_google_client_headers(&target, auth_headers, &options)
        .expect("explicit key overrides client auth");
    assert_eq!(headers["x-goog-api-key"], "explicit-key");
}

/// Pins the replacement-payload conversion used after pi
/// `src/api/google-generative-ai.ts:88-93` and `src/api/google-vertex.ts:106-111`.
#[test]
fn replacement_payload_preserves_generated_request_and_vertex_resource_semantics() {
    let studio = google_wire_request_from_params(
        &json!({
            "model": "gemini-3-flash-preview",
            "contents": "Hello",
            "config": {
                "systemInstruction": "Be useful",
                "responseLogprobs": true,
                "logprobs": 4,
                "responseSchema": {
                    "type": "object",
                    "properties": { "answer": { "type": ["string", "null"] } },
                    "additionalProperties": false
                },
                "tools": [{
                    "googleSearch": {
                        "searchTypes": ["WEB_SEARCH"],
                        "timeRangeFilter": { "startTime": "2026-01-01T00:00:00Z" }
                    },
                    "googleMaps": {
                        "authConfig": { "apiKey": "maps-key" },
                        "enableWidget": true
                    }
                }]
            }
        }),
        &GoogleRequestTarget::mldev(),
    )
    .expect("Gemini API request");
    assert_eq!(studio.model, "models/gemini-3-flash-preview");
    assert_eq!(studio.body["contents"][0]["role"], "user");
    assert_eq!(studio.body["contents"][0]["parts"][0]["text"], "Hello");
    assert_eq!(studio.body["systemInstruction"]["role"], "user");
    assert_eq!(studio.body["generationConfig"]["responseLogprobs"], true);
    assert_eq!(studio.body["generationConfig"]["logprobs"], 4);
    assert_eq!(
        studio.body["generationConfig"]["responseSchema"]["properties"]["answer"],
        json!({ "nullable": true, "type": "STRING" })
    );
    assert!(
        studio.body["generationConfig"]["responseSchema"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(
        studio.body["tools"][0]["googleMaps"]["authConfig"]["apiKey"],
        "maps-key"
    );

    let vertex_target =
        GoogleRequestTarget::vertex(Some("project-id".to_owned()), Some("eu".to_owned()));
    let vertex = google_wire_request_from_params(
        &json!({
            "model": "anthropic/claude-sonnet-4-5",
            "contents": [{ "role": "user", "parts": [{ "text": "Hello" }] }],
            "config": {
                "imageConfig": {
                    "outputMimeType": "image/png",
                    "imageOutputOptions": { "mimeType": "image/webp" }
                }
            }
        }),
        &vertex_target,
    )
    .expect("Vertex request");
    assert_eq!(
        vertex.model,
        "publishers/anthropic/models/claude-sonnet-4-5"
    );
    assert_eq!(
        vertex.body["generationConfig"]["imageConfig"]["imageOutputOptions"]["mimeType"],
        "image/webp"
    );

    let backend = create_google_backend(
        &model("gemini-3-flash-preview"),
        HeaderMap::new(),
        &StreamOptions::default(),
        "https://aiplatform.eu.rep.googleapis.com".to_owned(),
        "v1".to_owned(),
        false,
        vertex_target,
    )
    .expect("Vertex backend");
    assert_eq!(
        backend.request_url(&vertex).expect("request URL").as_str(),
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/project-id/locations/eu/publishers/anthropic/models/claude-sonnet-4-5:streamGenerateContent?alt=sse"
    );
}

/// Pins the per-request HTTP override passed through pi's payload hook at
/// `src/api/google-generative-ai.ts:88-93`.
#[test]
fn replacement_payload_http_options_override_url_and_deep_merge_body() {
    let request = google_wire_request_from_params(
        &json!({
            "model": "gemini-3-flash-preview",
            "contents": "Hello",
            "config": {
                "httpOptions": {
                    "baseUrl": "https://proxy.example.com/gemini",
                    "apiVersion": "",
                    "baseUrlResourceScope": "COLLECTION",
                    "headers": { "x-hook": "present" },
                    "timeout": 1500.5,
                    "extraBody": {
                        "generationConfig": { "temperature": 0.75 },
                        "hookField": true
                    }
                }
            }
        }),
        &GoogleRequestTarget::mldev(),
    )
    .expect("request");
    assert_eq!(request.http_options.timeout_ms, Some(1500.5));
    assert_eq!(request.http_options.headers["x-hook"], "present");
    let backend = create_google_backend(
        &model("gemini-3-flash-preview"),
        HeaderMap::new(),
        &StreamOptions::default(),
        "https://generativelanguage.googleapis.com".to_owned(),
        "v1beta".to_owned(),
        false,
        GoogleRequestTarget::mldev(),
    )
    .expect("backend");
    assert_eq!(
        backend.request_url(&request).expect("request URL").as_str(),
        "https://proxy.example.com/gemini/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
    );
    let mut body = request.body.clone();
    deep_merge_json(
        &mut body,
        request
            .http_options
            .extra_body
            .as_ref()
            .expect("extra body"),
    );
    assert_eq!(body["generationConfig"]["temperature"], 0.75);
    assert_eq!(body["hookField"], true);
}

/// Pins the signal branch at pi `src/api/google-generative-ai.ts:403-408`.
#[test]
fn payload_exposes_an_abort_snapshot_and_rejects_an_already_aborted_signal() {
    let controller = crate::utils::abort::AbortController::new();
    let mut options = GoogleOptions::default();
    options.stream.request.signal = Some(controller.signal());
    let params =
        build_params(&model("gemini-3-flash-preview"), &context(), &options).expect("live signal");
    assert_eq!(params["config"]["abortSignal"]["aborted"], false);
    controller.abort(crate::utils::abort::AbortReason::default_abort());
    assert_eq!(
        build_params(&model("gemini-3-flash-preview"), &context(), &options),
        Err("Request aborted".to_owned())
    );
}
