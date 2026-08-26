use agentprism_ai::{
    ApiFamily, ApiId, ApiKeyCredential, AssistantEvent, AuthResolutionOverrides,
    BEDROCK_REDACTED_REASONING_KIND, BEDROCK_THINKING_SIGNATURE_KIND, BedrockConverseStream,
    BedrockHandoff, BedrockOptions, BedrockToolCallIdPolicy, CacheRetention, CancellationToken,
    ConstrainedSampling, ConstrainedSamplingConfig, ContentBlock, ContentBlockId, Context,
    Credential, CredentialStore, DefaultRetryClassifier, EncodeContext, ErasedApiFullOptions,
    HeaderTransform, HeaderTransformContext, HttpRequest, HttpTransport, InMemoryCredentialStore,
    JsonSchemaStrictMode, LocalBoxFuture, LocalCredentialStore, LocalDefaultRetryClassifier,
    LocalHeaderTransform, LocalHttpTransport, LocalInMemoryCredentialStore, LocalModels,
    LocalResolveAuthRequest, LocalResolvedApiRequest, LocalResponseObserver, MapAuthContext,
    Message, MessageId, MiddlewareError, ModelDescriptor, ModelFingerprint, ModelId, ModelRequest,
    Models, OrderedJsonWriter, ProviderId, ProviderResponseMetadata, ReasoningLevel,
    ReplayCompleteness, ResolveAuthRequest, ResolvedApiRequest, ResolvedAuth,
    ResponseObservationContext, ResponseObserver, RetryPolicy, SecretString, SendBoxFuture,
    SimpleGenerationOptions, Timestamp, ToolCallId, ToolCallIdPolicy, ToolResultContent,
    ToolResultMessage, ToolSpec, TransportError, TypedModelDescriptor, UserMessage,
    transform_context_for_model,
};
use agentprism_bedrock::{
    BedrockConverseDecoder, BedrockDecodeContext, BedrockProviderFailure, BedrockSigner,
    BedrockSignerError, BedrockSignerResponse, BedrockSignerTransport, BedrockSigningConfig,
    LocalBedrockSigner, LocalBedrockSignerResponse, LocalBedrockSignerTransport,
    bedrock_converse_stream_api, bedrock_models, bedrock_provider,
    local_bedrock_converse_stream_api, local_bedrock_provider,
};
use futures_executor::block_on;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use url::Url;

const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
const FIXTURE_TOOL_CALL_1: &str = "call_fixture_0001";
const FIXTURE_TOOL_CALL_2: &str = "call_fixture_0002";

fn model(id: &str) -> ModelDescriptor {
    bedrock_models()
        .into_iter()
        .find(|model| model.common.model_ref.model.as_str() == id)
        .expect("fixture Bedrock model")
}

fn typed(model: &ModelDescriptor) -> TypedModelDescriptor<BedrockConverseStream> {
    let agentprism_ai::ApiModelConfig::BedrockConverse(config) = &model.api else {
        panic!("Bedrock model config")
    };
    TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    }
}

fn user(id: &str, text: &str) -> Message {
    Message::User(UserMessage {
        id: MessageId::new(id),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-text")),
            text: text.to_owned(),
        }],
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    })
}

fn one_user_context() -> Context {
    let mut context = Context::new(None);
    context.messages.push(user("user-1", "hello"));
    context
}

fn encode_command(model: &ModelDescriptor, context: &Context, options: &BedrockOptions) -> String {
    try_encode_command(model, context, options).expect("Bedrock encoding")
}

fn try_encode_command(
    model: &ModelDescriptor,
    context: &Context,
    options: &BedrockOptions,
) -> Result<String, agentprism_ai::EncodeError> {
    let projected =
        transform_context_for_model(context, model, &Default::default(), &BedrockHandoff)
            .expect("Bedrock handoff")
            .context;
    let typed = typed(model);
    let agentprism_ai::ApiModelConfig::BedrockConverse(config) = &model.api else {
        unreachable!()
    };
    let value = BedrockConverseStream::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &config.compat,
            effective_base_url: &model.common.base_url,
        },
        options,
    )?;
    Ok(OrderedJsonWriter::stringify(&value.into()).expect("ordered JSON"))
}

fn final_http_body(command: String) -> Vec<u8> {
    let capture = Arc::new(SendCapture::default());
    block_on(BedrockSignerTransport::new(capture.clone()).execute(
        HttpRequest {
            body: command.into_bytes(),
            ..logical_request(HeaderMap::new())
        },
        CancellationToken::new(),
    ))
    .expect("serialize Bedrock request");
    capture.requests.lock().expect("capture lock")[0]
        .body
        .clone()
}

fn decode_reasoning(
    model: &ModelDescriptor,
    deltas: &[serde_json::Value],
    stop: bool,
) -> Vec<AssistantEvent> {
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-1"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::from_unix_millis(1_700_000_000_001),
    });
    let mut events = decoder.take_events();
    events.extend(decoder.push_event("messageStart", &json!({"role":"assistant"})));
    for delta in deltas {
        events.extend(decoder.push_event(
            "contentBlockDelta",
            &json!({"contentBlockIndex":0,"delta":{"reasoningContent":delta}}),
        ));
    }
    if stop {
        events.extend(decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0})));
        events.extend(decoder.push_event("messageStop", &json!({"stopReason":"end_turn"})));
        events.extend(decoder.finish());
    }
    events
}

fn terminal(events: &[AssistantEvent]) -> agentprism_ai::AssistantMessage {
    events
        .iter()
        .find_map(AssistantEvent::terminal_message)
        .cloned()
        .expect("terminal assistant message")
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `bedrock-converse-stream.ts:635-670`.
#[test]
fn bedrock_redacted_chunks_concatenate_as_bytes() {
    let model = model("openai.gpt-5.6-terra");
    let message = terminal(&decode_reasoning(
        &model,
        &[
            json!({"redactedContent":[1,2]}),
            json!({"redactedContent":[3,255]}),
        ],
        true,
    ));
    let item = message
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == BEDROCK_REDACTED_REASONING_KIND)
        .expect("redacted replay item");
    assert_eq!(item.as_bytes(), Some([1, 2, 3, 255].as_slice()));
    assert_eq!(item.completeness, ReplayCompleteness::Complete);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking { text, redacted: true, .. } if text == "[Reasoning redacted]"
    ));
}

/// Architecture v2 part 2 §1.7 and §10.2 (R1/R8); pinned Pi basis:
/// `bedrock-redacted-reasoning.test.ts` and `bedrock-converse-stream.ts:664-670`.
#[test]
fn bedrock_redacted_bytes_survive_json_round_trip() {
    let model = model("openai.gpt-5.6-terra");
    let original = terminal(&decode_reasoning(
        &model,
        &[json!({"redactedContent":"AQL/gA=="})],
        true,
    ));
    let json = serde_json::to_string(&original).expect("persist assistant message");
    assert!(!json.contains("redactedChunks"));
    let restored: agentprism_ai::AssistantMessage =
        serde_json::from_str(&json).expect("restore assistant message");
    assert_eq!(restored, original);
    assert_eq!(
        restored.replay.items[0].as_bytes(),
        Some([1, 2, 255, 128].as_slice())
    );
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `bedrock-redacted-reasoning.test.ts` turn-two replay case.
#[test]
fn bedrock_turn_two_replays_redacted_content_bytes() {
    let model = model("openai.gpt-5.6-terra");
    let assistant = terminal(&decode_reasoning(
        &model,
        &[
            json!({"redactedContent":[1,2]}),
            json!({"redactedContent":[3,255]}),
        ],
        true,
    ));
    let assistant: agentprism_ai::AssistantMessage =
        serde_json::from_str(&serde_json::to_string(&assistant).expect("serialize assistant"))
            .expect("deserialize assistant");
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(
        &model,
        &context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#"{"reasoningContent":{"redactedContent":"AQID/w=="}}"#));
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `bedrock-converse-stream.ts:626-641,1002-1019`.
#[test]
fn bedrock_signed_reasoning_replays_text_and_signature() {
    let model = model("us.anthropic.claude-opus-4-8");
    let events = decode_reasoning(
        &model,
        &[
            json!({"text":"think"}),
            json!({"signature":"sig-"}),
            json!({"signature":"one"}),
        ],
        true,
    );
    let fragments = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ReplayData {
                operation: agentprism_ai::ReplayDataOperation::AppendUtf8(fragment),
                ..
            } => Some(fragment.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(fragments, ["sig-", "one"]);
    let assistant = terminal(&events);
    assert_eq!(
        assistant.replay.items[0].kind.as_str(),
        BEDROCK_THINKING_SIGNATURE_KIND
    );
    assert_eq!(assistant.replay.items[0].as_utf8(), Some("sig-one"));
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(
        &model,
        &context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(
        r#"{"reasoningContent":{"reasoningText":{"text":"think","signature":"sig-one"}}}"#
    ));
}

/// Architecture v2 part 2 §1.7, §1.9 R2–R4, and §10.2/§10.8;
/// pinned Pi basis: `bedrock-converse-stream.ts:635-657`, where the first
/// `redactedContent` chunk clears a signature accumulated earlier for the same
/// reasoning block.
#[test]
fn bedrock_signature_then_redacted_turn_two_pi_exact() {
    let model = model("openai.gpt-5.6-terra");
    let events = decode_reasoning(
        &model,
        &[
            json!({"signature":"superseded-signature"}),
            json!({"redactedContent":[1,2]}),
            json!({"redactedContent":[3,255]}),
        ],
        true,
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AssistantEvent::ReplayItemDiscarded { .. }))
    );

    let assistant = terminal(&events);
    assert_eq!(assistant.replay.items.len(), 1);
    assert_eq!(
        assistant.replay.items[0].kind.as_str(),
        BEDROCK_REDACTED_REASONING_KIND
    );
    assert_eq!(
        assistant.replay.items[0].as_bytes(),
        Some([1, 2, 3, 255].as_slice())
    );
    let assistant: agentprism_ai::AssistantMessage =
        serde_json::from_str(&serde_json::to_string(&assistant).expect("persist assistant"))
            .expect("restore assistant");
    assert_eq!(assistant.replay.items.len(), 1);

    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    assert_eq!(
        final_http_body(encode_command(
            &model,
            &context,
            &BedrockOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        )),
        br#"{"messages":[{"role":"user","content":[{"text":"hello"}]},{"role":"assistant","content":[{"reasoningContent":{"redactedContent":"AQID/w=="}}]},{"role":"user","content":[{"text":"continue"}]}],"inferenceConfig":{}}"#
    );
}

/// Architecture v2 part 2 §1.3 and §1.9 R3; pinned Pi basis:
/// `bedrock-converse-stream.ts:626-641`, which appends every observed
/// `reasoningContent.signature` delta before the block stops.
#[test]
fn bedrock_failed_signed_reasoning_retains_incomplete_replay() {
    let model = model("us.anthropic.claude-opus-4-8");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("failed-signed-reasoning"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let events = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig-fragment"}}}),
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ReplayData {
            operation: agentprism_ai::ReplayDataOperation::AppendUtf8(fragment),
            ..
        } if fragment == "sig-fragment"
    )));

    let message = terminal(&decoder.fail_transport("stream", "connection lost"));
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(
        message.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    );
    assert_eq!(message.replay.items[0].as_utf8(), Some("sig-fragment"));
}

/// Architecture v2 part 2 §1.3 and §1.9 R3; pinned Pi basis:
/// `bedrock-converse-stream.ts:626-641` and the aborted terminal path.
#[test]
fn bedrock_cancelled_signed_reasoning_retains_incomplete_replay() {
    let model = model("us.anthropic.claude-opus-4-8");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("cancelled-signed-reasoning"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig-fragment"}}}),
    );

    let message = terminal(&decoder.cancel("Request was aborted"));
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(
        message.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    );
    assert_eq!(message.replay.items[0].as_utf8(), Some("sig-fragment"));
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `bedrock-converse-stream.ts:1006-1011`.
#[test]
fn bedrock_missing_required_signature_falls_back_to_text() {
    let model = model("us.anthropic.claude-opus-4-8");
    let assistant = terminal(&decode_reasoning(&model, &[json!({"text":"think"})], true));
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(&model, &context, &BedrockOptions::default());
    assert!(body.contains(r#"{"text":"think"}"#));
    assert!(!body.contains("reasoningText"));
}

/// Architecture v2 part 2 §1.7 and §10.2; pinned Pi basis:
/// `bedrock-converse-stream.ts:1002-1027`.
#[test]
fn bedrock_non_anthropic_model_omits_reasoning_signature() {
    let model = model("openai.gpt-5.6-terra");
    let assistant = terminal(&decode_reasoning(
        &model,
        &[
            json!({"text":"think"}),
            json!({"signature":"not-supported"}),
        ],
        true,
    ));
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(&model, &context, &BedrockOptions::default());
    assert!(body.contains(r#"{"reasoningText":{"text":"think"}}"#));
    assert!(!body.contains("not-supported"));
}

/// Architecture v2 part 2 §1.7 and §10.2 (R2); pinned Pi basis:
/// `bedrock-redacted-reasoning.test.ts` partial-stream behavior.
#[test]
fn bedrock_partial_redacted_payload_is_not_replayed() {
    let model = model("openai.gpt-5.6-terra");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-partial"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event("messageStart", &json!({"role":"assistant"}));
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":[9,8]}}}),
    );
    let failed = terminal(&decoder.finish());
    assert_eq!(
        failed.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    );
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(failed));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(&model, &context, &BedrockOptions::default());
    assert!(!body.contains("redactedContent"));
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `bedrock-converse-stream.ts:570-727`. Unknown union members, unsupported
/// starts/deltas, kind-mismatched deltas, tool deltas without a start, and
/// stops for unknown blocks are all ignored rather than made protocol-fatal.
#[test]
fn bedrock_stream_permissive_variant_handling_matches_pi() {
    let model = model("us.anthropic.claude-opus-4-8");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-permissive"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();

    assert!(decoder.push_event("futureEvent", &json!(7)).is_empty());
    assert!(
        decoder
            .push_event(
                "contentBlockStart",
                &json!({"contentBlockIndex":0,"start":{"guardContent":{}}}),
            )
            .is_empty()
    );
    assert!(
        decoder
            .push_event(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"guardContent":{}}}),
            )
            .is_empty()
    );
    assert!(
        decoder
            .push_event(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}),
            )
            .is_empty()
    );

    let events = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"text":"hello"}}),
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::TextDelta { delta, .. } if delta == "hello"
    )));
    assert!(
        decoder
            .push_event(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"ignored"}}}),
            )
            .is_empty()
    );
    assert!(
        decoder
            .push_event(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"ignored"}}}),
            )
            .is_empty()
    );
    assert!(
        decoder
            .push_event("contentBlockStop", &json!({"contentBlockIndex":99}),)
            .is_empty()
    );
    let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
    let _ = decoder.push_event("messageStop", &json!({"stopReason":"end_turn"}));
    let message = terminal(&decoder.finish());
    assert_eq!(message.content.len(), 1);
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "hello"
    ));

    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-empty-tool-metadata"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let events = decoder.push_event(
        "contentBlockStart",
        &json!({"contentBlockIndex":0,"start":{"toolUse":{}}}),
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ToolCallMetadata { call_id, name, .. }
            if call_id.as_str().is_empty() && name.as_deref() == Some("")
    )));
}

/// Architecture v2 part 2 §2.1 and §10.1; pinned Pi basis:
/// `bedrock-converse-stream.ts:279-310,570-604`. Modeled stream members are
/// thrown as plain objects, so Pi JSON-stringifies them without an event-name
/// prefix.
#[test]
fn bedrock_modeled_stream_failure_message_matches_pi() {
    let model = model("us.anthropic.claude-opus-4-8");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-modeled-failure"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing,
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let message =
        terminal(&decoder.push_event("validationException", &json!({"message":"invalid request"})));
    let error = message.finish.error.expect("modeled stream failure");
    assert_eq!(error.message, r#"{"message":"invalid request"}"#);
    assert_eq!(error.provider_code, None);
}

fn decoded_tool_turn(model: &ModelDescriptor, arguments: &str) -> agentprism_ai::AssistantMessage {
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("assistant-tool"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event("messageStart", &json!({"role":"assistant"}));
    let _ = decoder.push_event(
        "contentBlockStart",
        &json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tool-1","name":"edit"}}}),
    );
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":arguments}}}),
    );
    let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
    let _ = decoder.push_event("messageStop", &json!({"stopReason":"tool_use"}));
    terminal(&decoder.finish())
}

fn decoded_text_turn(
    model: &ModelDescriptor,
    message_id: &str,
    text: &str,
) -> agentprism_ai::AssistantMessage {
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new(message_id),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event("messageStart", &json!({"role":"assistant"}));
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"text":text}}),
    );
    let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
    let _ = decoder.push_event("messageStop", &json!({"stopReason":"end_turn"}));
    terminal(&decoder.finish())
}

/// Architecture v2 part 2 §1.2 and §10.1; pinned Pi basis:
/// `bedrock-convert-messages.test.ts` streamed-tool-argument case.
#[test]
fn bedrock_streamed_tool_arguments_preserve_empty_keys() {
    let model = model("us.anthropic.claude-opus-4-8");
    let message = decoded_tool_turn(
        &model,
        r#"{"path":"/workspace/file.js","edits":[{"oldText":"x","newText":"y","":""}]}"#,
    );
    let ContentBlock::ToolCall { call, .. } = &message.content[0] else {
        panic!("decoded tool call")
    };
    assert_eq!(call.arguments["edits"][0][""], "");
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `bedrock-convert-messages.test.ts` replay sanitation case.
#[test]
fn bedrock_replayed_tool_arguments_remove_empty_keys() {
    let model = model("us.anthropic.claude-opus-4-8");
    let assistant = decoded_tool_turn(
        &model,
        r#"{"path":"/workspace/file.js","edits":[{"oldText":"x","newText":"y","":""}]}"#,
    );
    let mut context = Context::new(None);
    context.messages.push(Message::Assistant(assistant));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("result-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            tool_name: "edit".to_owned(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("result-text"),
                text: "done".to_owned(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::default(),
        }));
    context.messages.push(user("user-2", "continue"));
    let body = encode_command(
        &model,
        &context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(
        r#""input":{"path":"/workspace/file.js","edits":[{"oldText":"x","newText":"y"}]}"#
    ));
    assert!(!body.contains(r#""":"""#));
}

fn normalize_bedrock_tool_call_id(id: &str) -> String {
    let fingerprint = ModelFingerprint::new(
        "amazon-bedrock",
        "bedrock-converse-stream",
        "us.anthropic.claude-opus-4-8",
    );
    BedrockToolCallIdPolicy
        .normalize(&ToolCallId::new(id), &fingerprint, &fingerprint)
        .expect("Bedrock tool-call ID normalization")
        .as_str()
        .to_owned()
}

/// Architecture v2 part 2 §4.3 phase 6; pinned Pi basis:
/// `bedrock-converse-stream.ts:886-888`. Its non-`u` regular expression
/// replaces each UTF-16 surrogate code unit independently.
#[test]
fn bedrock_tool_call_id_astral_utf16_pi_exact() {
    assert_eq!(normalize_bedrock_tool_call_id("a😀b"), "a__b");
}

/// Architecture v2 part 2 §4.3 phase 6; pinned Pi basis:
/// `bedrock-converse-stream.ts:886-888`. Replacement precedes UTF-16
/// `.length`/`.slice`, so both surrogate replacements count at the boundary.
#[test]
fn bedrock_tool_call_id_64_utf16_units_pi_exact() {
    let prefix = "a".repeat(62);
    let normalized = normalize_bedrock_tool_call_id(&format!("{prefix}😀b"));

    assert_eq!(normalized, format!("{prefix}__"));
    assert_eq!(normalized.encode_utf16().count(), 64);
}

/// Architecture v2 part 2 §3 and §10.8; pinned Pi basis:
/// `bedrock-convert-messages.test.ts` blank-content and filtered-block cases.
/// Rust's typed canonical enum makes the test's injected unknown TypeScript
/// variants unrepresentable; the remaining observable empty-content rules are
/// preserved here.
#[test]
fn bedrock_message_conversion_edge_cases_match_pi() {
    let model = model("us.anthropic.claude-opus-4-8");

    let mut user_context = Context::new(None);
    user_context.messages.push(Message::User(UserMessage {
        id: MessageId::new("blank-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("blank-user-text"),
            text: "   ".to_owned(),
        }],
        timestamp: Timestamp::default(),
    }));
    let body = encode_command(
        &model,
        &user_context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""content":[{"text":"<empty>"}]"#));

    let mut mixed_user_context = Context::new(None);
    mixed_user_context.messages.push(Message::User(UserMessage {
        id: MessageId::new("mixed-user"),
        content: vec![
            ContentBlock::Text {
                id: ContentBlockId::new("mixed-user-blank"),
                text: "\t\n".to_owned(),
            },
            ContentBlock::Image {
                id: ContentBlockId::new("mixed-user-image"),
                data: PNG_1X1.to_owned(),
                mime_type: "image/png".to_owned(),
            },
        ],
        timestamp: Timestamp::default(),
    }));
    let body = encode_command(
        &model,
        &mixed_user_context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""content":[{"image":{"format":"png""#));
    assert!(!body.contains("<empty>"));

    let mut assistant_context = Context::new(None);
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("blank-assistant"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"text":""}}),
    );
    let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
    let _ = decoder.push_event("messageStop", &json!({"stopReason":"end_turn"}));
    assistant_context
        .messages
        .push(Message::Assistant(terminal(&decoder.finish())));
    assert_eq!(
        encode_command(&model, &assistant_context, &BedrockOptions::default()),
        r#"{"modelId":"us.anthropic.claude-opus-4-8","messages":[],"inferenceConfig":{"maxTokens":128000}}"#
    );

    let mut tool_context = Context::new(None);
    tool_context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("blank-tool-result"),
            tool_call_id: ToolCallId::new("tool-1"),
            tool_name: "lookup".to_owned(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("blank-tool-result-text"),
                text: String::new(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::default(),
        }));
    let body = encode_command(
        &model,
        &tool_context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""content":[{"text":"<empty>"}],"status":"success""#));
}

/// Architecture v2 part 2 §10.8 Bedrock wire conformance; pinned Pi basis:
/// `packages/ai/test/empty.test.ts` Bedrock Converse matrix.
#[test]
fn bedrock_empty_message_matrix_pi_exact() {
    let model = model("us.anthropic.claude-opus-4-8");
    let options = BedrockOptions {
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };
    for (id, content) in [
        ("empty-array", Vec::new()),
        (
            "empty-string",
            vec![ContentBlock::Text {
                id: ContentBlockId::new("empty-string-text"),
                text: String::new(),
            }],
        ),
        (
            "whitespace",
            vec![ContentBlock::Text {
                id: ContentBlockId::new("whitespace-text"),
                text: "   ".into(),
            }],
        ),
    ] {
        let mut context = Context::new(None);
        context.messages.push(Message::User(UserMessage {
            id: MessageId::new(id),
            content,
            timestamp: Timestamp::default(),
        }));
        assert!(
            encode_command(&model, &context, &options).contains(r#""text":"<empty>""#),
            "{id}"
        );
    }

    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("first-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("first-user-text"),
            text: "First message".into(),
        }],
        timestamp: Timestamp::default(),
    }));
    context.messages.push(Message::Assistant(decoded_text_turn(
        &model,
        "empty-assistant",
        "",
    )));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("second-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("second-user-text"),
            text: "Second message".into(),
        }],
        timestamp: Timestamp::default(),
    }));
    let body = encode_command(&model, &context, &options);
    assert_eq!(body.matches(r#""role":"user""#).count(), 2);
    assert!(!body.contains(r#""role":"assistant""#));
}

/// Architecture v2 part 2 §3 and §10.8; pinned Pi basis:
/// `bedrock-converse-stream.ts:891-913,979-990`. Pi's blank checks use
/// ECMAScript `String.prototype.trim()`: U+FEFF is whitespace and U+0085 is
/// not. User, assistant, and tool-result text must all preserve that split.
#[test]
fn bedrock_ecmascript_trim_message_blocks_pi_exact() {
    let model = model("us.anthropic.claude-opus-4-8");
    let options = BedrockOptions {
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };

    for (text, expected) in [("\u{feff}", "<empty>"), ("\u{0085}", "\u{0085}")] {
        let mut context = Context::new(None);
        context.messages.push(Message::User(UserMessage {
            id: MessageId::new("unicode-user"),
            content: vec![ContentBlock::Text {
                id: ContentBlockId::new("unicode-user-text"),
                text: text.to_owned(),
            }],
            timestamp: Timestamp::default(),
        }));
        let body = encode_command(&model, &context, &options);
        assert!(body.contains(expected), "user body: {body}");
    }

    for (text, retained) in [("\u{feff}", false), ("\u{0085}", true)] {
        let mut context = Context::new(None);
        context.messages.push(Message::Assistant(decoded_text_turn(
            &model,
            "unicode-assistant",
            text,
        )));
        let body = encode_command(&model, &context, &options);
        assert_eq!(body.contains(text), retained, "assistant body: {body}");
        assert_eq!(body.contains(r#""role":"assistant""#), retained);
    }

    for (text, expected) in [("\u{feff}", "<empty>"), ("\u{0085}", "\u{0085}")] {
        let mut context = Context::new(None);
        context
            .messages
            .push(Message::ToolResult(ToolResultMessage {
                id: MessageId::new("unicode-tool-result"),
                tool_call_id: ToolCallId::new("unicode-tool-call"),
                tool_name: "lookup".to_owned(),
                content: vec![ToolResultContent::Text {
                    id: ContentBlockId::new("unicode-tool-text"),
                    text: text.to_owned(),
                }],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: Timestamp::default(),
            }));
        let body = encode_command(&model, &context, &options);
        assert!(body.contains(expected), "tool-result body: {body}");
    }
}

/// Architecture v2 part 2 §1.7 and §10.2/§10.8; pinned Pi basis:
/// `bedrock-converse-stream.ts:991-1011`. Both visible reasoning and its
/// signature use ECMAScript trim semantics before a signed block is replayed.
#[test]
fn bedrock_ecmascript_trim_reasoning_and_signature_pi_exact() {
    let model = model("us.anthropic.claude-opus-4-8");
    let options = BedrockOptions {
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };

    for (thinking, retained) in [("\u{feff}", false), ("\u{0085}", true)] {
        let assistant = terminal(&decode_reasoning(
            &model,
            &[json!({"text":thinking}), json!({"signature":"signature"})],
            true,
        ));
        let mut context = Context::new(None);
        context.messages.push(Message::Assistant(assistant));
        let body = encode_command(&model, &context, &options);
        assert_eq!(body.contains("reasoningText"), retained, "body: {body}");
        assert_eq!(body.contains(thinking), retained, "body: {body}");
    }

    for (signature, signed) in [("\u{feff}", false), ("\u{0085}", true)] {
        let assistant = terminal(&decode_reasoning(
            &model,
            &[json!({"text":"think"}), json!({"signature":signature})],
            true,
        ));
        let mut context = Context::new(None);
        context.messages.push(Message::Assistant(assistant));
        let body = encode_command(&model, &context, &options);
        assert_eq!(body.contains("reasoningText"), signed, "body: {body}");
        assert_eq!(
            body.contains(r#"{"text":"think"}"#),
            !signed,
            "body: {body}"
        );
        assert_eq!(body.contains(signature), signed, "body: {body}");
    }
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `bedrock-converse-stream.ts:1266-1295`. Pi calls `atob` for user and
/// tool-result images, and Smithy serializes those bytes back to canonical
/// padded base64. Invalid input fails request encoding.
#[test]
fn bedrock_user_and_tool_result_images_use_pi_atob_pi_exact() {
    let model = model("us.anthropic.claude-opus-4-8");
    let options = BedrockOptions {
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };

    let mut user_context = Context::new(None);
    user_context.messages.push(Message::User(UserMessage {
        id: MessageId::new("atob-user"),
        content: vec![ContentBlock::Image {
            id: ContentBlockId::new("atob-user-image"),
            // Infra ASCII whitespace is ignored and nonzero discarded pad bits
            // are accepted: atob("Z h==") is one byte `f`, canonical `Zg==`.
            data: "Z h==".to_owned(),
            mime_type: "image/png".to_owned(),
        }],
        timestamp: Timestamp::default(),
    }));
    let body = encode_command(&model, &user_context, &options);
    assert!(body.contains(r#""bytes":"Zg==""#), "user body: {body}");
    assert!(!body.contains("Z h=="));

    let mut tool_context = Context::new(None);
    tool_context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("atob-tool-result"),
            tool_call_id: ToolCallId::new("atob-tool-call"),
            tool_name: "lookup".to_owned(),
            content: vec![ToolResultContent::Image {
                id: ContentBlockId::new("atob-tool-image"),
                // Missing padding and ASCII whitespace are accepted by atob.
                data: "A QI\n".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::default(),
        }));
    let body = encode_command(&model, &tool_context, &options);
    assert!(body.contains(r#""bytes":"AQI=""#), "tool body: {body}");
    assert!(!body.contains("A QI"));

    let Message::User(message) = &mut user_context.messages[0] else {
        unreachable!()
    };
    let ContentBlock::Image { data, .. } = &mut message.content[0] else {
        unreachable!()
    };
    *data = "AQ\u{000b}ID".to_owned();
    assert!(matches!(
        try_encode_command(&model, &user_context, &options),
        Err(agentprism_ai::EncodeError::InvalidRequest { .. })
    ));

    {
        let Message::ToolResult(message) = &mut tool_context.messages[0] else {
            unreachable!()
        };
        let ToolResultContent::Image { data, .. } = &mut message.content[0] else {
            unreachable!()
        };
        *data = "AQ===".to_owned();
    }
    assert!(matches!(
        try_encode_command(&model, &tool_context, &options),
        Err(agentprism_ai::EncodeError::InvalidRequest { .. })
    ));

    // Infra forgiving-base64 accepts omitted padding, but a present padding
    // suffix is stripped only when the whitespace-free input length is a
    // multiple of four. ECMAScript atob("Zg=") therefore rejects.
    let Message::ToolResult(message) = &mut tool_context.messages[0] else {
        unreachable!()
    };
    let ToolResultContent::Image { data, .. } = &mut message.content[0] else {
        unreachable!()
    };
    *data = "Zg=".to_owned();
    assert!(matches!(
        try_encode_command(&model, &tool_context, &options),
        Err(agentprism_ai::EncodeError::InvalidRequest { .. })
    ));
}

/// Architecture v2 part 2 §5.1 and §10.8; pinned Pi basis:
/// `bedrock-convert-messages.test.ts` constrained-sampling case.
#[test]
fn bedrock_strict_tool_schema_matches_capability() {
    let strict_model = model("us.anthropic.claude-haiku-4-5-20251001-v1:0");
    let mut context = one_user_context();
    context.tools.push(ToolSpec {
        schema_version: 1,
        name: "lookup".to_owned(),
        description: "Look up a value".to_owned(),
        parameters: json!({
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"]
        }),
        constrained_sampling: Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: JsonSchemaStrictMode::Require,
            },
        )),
    });
    let body = encode_command(
        &strict_model,
        &context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""strict":true"#));
    assert!(body.contains(r#""additionalProperties":false"#));

    let unsupported = model("amazon.nova-lite-v1:0");
    assert!(
        BedrockConverseStream::encode(
            EncodeContext {
                model: &typed(&unsupported),
                context: &context,
                compat: match &unsupported.api {
                    agentprism_ai::ApiModelConfig::BedrockConverse(config) => &config.compat,
                    _ => unreachable!(),
                },
                effective_base_url: &unsupported.common.base_url,
            },
            &BedrockOptions::default(),
        )
        .is_err()
    );
}

/// Architecture v2 part 2 §3 and §10.8; pinned Pi basis:
/// `bedrock-convert-messages.test.ts` and `transform-messages.ts`.
#[test]
fn bedrock_wire_tool_results_orphan_and_failed_turns_pi_exact() {
    let model = model("us.anthropic.claude-opus-4-8");
    let assistant = decoded_tool_turn(&model, r#"{"path":"/workspace/file.js"}"#);
    let mut context = Context::new(None);
    context.messages.push(Message::Assistant(assistant));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("result-1"),
            tool_call_id: ToolCallId::new("tool-1"),
            tool_name: "edit".to_owned(),
            content: vec![
                ToolResultContent::Text {
                    id: ContentBlockId::new("result-text"),
                    text: "failed".to_owned(),
                },
                ToolResultContent::Image {
                    id: ContentBlockId::new("result-image"),
                    data: "AQID".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: true,
            timestamp: Timestamp::default(),
        }));
    let body = final_http_body(encode_command(
        &model,
        &context,
        &BedrockOptions {
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    ));
    assert_eq!(
        body,
        br#"{"messages":[{"role":"assistant","content":[{"toolUse":{"toolUseId":"tool-1","name":"edit","input":{"path":"/workspace/file.js"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"tool-1","content":[{"text":"failed"},{"image":{"format":"png","source":{"bytes":"AQID"}}}],"status":"error"}}]}],"inferenceConfig":{"maxTokens":128000}}"#
    );

    let mut orphan = Context::new(None);
    orphan
        .messages
        .push(Message::Assistant(decoded_tool_turn(&model, r#"{}"#)));
    orphan.messages.push(user("user-after-orphan", "continue"));
    let body = encode_command(&model, &orphan, &BedrockOptions::default());
    assert!(body.contains("No result provided"));

    let mut failed_decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("failed-turn"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = failed_decoder.take_events();
    let _ = failed_decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"text":"omit me"}}),
    );
    let failed = terminal(&failed_decoder.fail_transport("stream", "failure"));
    let mut failed_context = Context::new(None);
    failed_context.messages.push(Message::Assistant(failed));
    failed_context
        .messages
        .push(user("user-after-failed", "kept"));
    let body = encode_command(&model, &failed_context, &BedrockOptions::default());
    assert!(!body.contains("omit me"));
    assert!(body.contains("kept"));
}

#[derive(Default)]
struct SendCapture {
    requests: Mutex<Vec<HttpRequest>>,
    configs: Mutex<Vec<BedrockSigningConfig>>,
}

impl BedrockSigner for SendCapture {
    fn execute(
        &self,
        config: BedrockSigningConfig,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>> {
        self.configs.lock().expect("capture lock").push(config);
        self.requests.lock().expect("capture lock").push(request);
        Box::pin(async { Ok(BedrockSignerResponse::empty(200, HeaderMap::new())) })
    }
}

#[derive(Default)]
struct LocalCapture {
    requests: RefCell<Vec<HttpRequest>>,
    configs: RefCell<Vec<BedrockSigningConfig>>,
}

impl LocalBedrockSigner for LocalCapture {
    fn execute(
        &self,
        config: BedrockSigningConfig,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalBedrockSignerResponse, BedrockSignerError>> {
        self.configs.borrow_mut().push(config);
        self.requests.borrow_mut().push(request);
        Box::pin(async { Ok(LocalBedrockSignerResponse::empty(200, HeaderMap::new())) })
    }
}

fn logical_request(headers: HeaderMap) -> HttpRequest {
    HttpRequest {
        method: Method::POST,
        url: Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com").expect("URL"),
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body: br#"{"modelId":"us.anthropic.claude-opus-4-8","messages":[],"inferenceConfig":{}}"#
            .to_vec(),
        timeout: None,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    }
}

fn resolved_transport_headers(resolved: &ResolvedAuth) -> HeaderMap {
    let mut headers = resolved.transport_headers.clone();
    for (name, value) in &resolved.headers {
        headers.insert(name, value.clone());
    }
    headers
}

fn send_resolved_auth_to_signer(
    capture: &Arc<SendCapture>,
    resolved: &ResolvedAuth,
) -> BedrockSigningConfig {
    let mut request = logical_request(resolved.headers.clone());
    request.auth_headers = resolved_transport_headers(resolved);
    if let Some(endpoint) = &resolved.base_url {
        request.url = endpoint.clone();
    }
    block_on(
        BedrockSignerTransport::new(capture.clone()).execute(request, CancellationToken::new()),
    )
    .expect("Bedrock signer request");
    assert!(
        capture.requests.lock().expect("capture lock")[0]
            .headers
            .get("x-pi-bedrock-signing-config")
            .is_none(),
        "the private credential carrier must never reach the signed request"
    );
    capture.configs.lock().expect("capture lock")[0].clone()
}

fn local_resolved_auth_to_signer(
    capture: &Rc<LocalCapture>,
    resolved: &ResolvedAuth,
) -> BedrockSigningConfig {
    let mut request = logical_request(resolved.headers.clone());
    request.auth_headers = resolved_transport_headers(resolved);
    if let Some(endpoint) = &resolved.base_url {
        request.url = endpoint.clone();
    }
    block_on(
        LocalBedrockSignerTransport::new(capture.clone())
            .execute(request, CancellationToken::new()),
    )
    .expect("local Bedrock signer request");
    assert!(
        capture.requests.borrow()[0]
            .headers
            .get("x-pi-bedrock-signing-config")
            .is_none(),
        "the private credential carrier must never reach the signed request"
    );
    capture.configs.borrow()[0].clone()
}

struct PrivateHeaderTransform(&'static str);

impl HeaderTransform for PrivateHeaderTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.insert(
            "x-pi-bedrock-signing-config",
            HeaderValue::from_static(self.0),
        );
        Box::pin(async { Ok(()) })
    }
}

impl LocalHeaderTransform for PrivateHeaderTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.insert(
            "x-pi-bedrock-signing-config",
            HeaderValue::from_static(self.0),
        );
        Box::pin(async { Ok(()) })
    }
}

struct ClonePrivateHeaderValueTransform(HeaderValue);

impl HeaderTransform for ClonePrivateHeaderValueTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        let replacement = self.0.clone();
        assert!(replacement.is_sensitive());
        assert!(
            headers.get("x-pi-bedrock-signing-config").is_none(),
            "the private carrier must remain outside mutable logical headers"
        );
        headers.insert("x-pi-bedrock-signing-config", replacement);
        Box::pin(async { Ok(()) })
    }
}

impl LocalHeaderTransform for ClonePrivateHeaderValueTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        let replacement = self.0.clone();
        assert!(replacement.is_sensitive());
        assert!(
            headers.get("x-pi-bedrock-signing-config").is_none(),
            "the private carrier must remain outside mutable logical headers"
        );
        headers.insert("x-pi-bedrock-signing-config", replacement);
        Box::pin(async { Ok(()) })
    }
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `bedrock-custom-headers.test.ts` and `bedrock-converse-stream.ts:456-474`.
#[test]
fn bedrock_custom_headers_are_inserted_before_signing() {
    let mut headers = HeaderMap::new();
    headers.insert("x-custom-signed", HeaderValue::from_static("present"));
    let send = Arc::new(SendCapture::default());
    block_on(
        BedrockSignerTransport::new(send.clone())
            .execute(logical_request(headers.clone()), CancellationToken::new()),
    )
    .expect("Send build stage");
    assert_eq!(
        send.requests.lock().expect("capture lock")[0]
            .headers
            .get("x-custom-signed"),
        Some(&HeaderValue::from_static("present"))
    );

    let local = Rc::new(LocalCapture::default());
    block_on(
        LocalBedrockSignerTransport::new(local.clone())
            .execute(logical_request(headers), CancellationToken::new()),
    )
    .expect("Local build stage");
    assert_eq!(
        local.requests.borrow()[0].headers.get("x-custom-signed"),
        Some(&HeaderValue::from_static("present"))
    );
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `bedrock-custom-headers.test.ts` reserved-header cases.
#[test]
fn bedrock_reserved_headers_are_suppressed() {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("caller"));
    headers.insert(header::HOST, HeaderValue::from_static("wrong.example"));
    headers.insert("x-amz-date", HeaderValue::from_static("caller-date"));
    headers.insert("x-safe", HeaderValue::from_static("kept"));
    let send = Arc::new(SendCapture::default());
    block_on(
        BedrockSignerTransport::new(send.clone())
            .execute(logical_request(headers.clone()), CancellationToken::new()),
    )
    .expect("Send build stage");
    let captured = send.requests.lock().expect("capture lock");
    assert!(captured[0].headers.get(header::AUTHORIZATION).is_none());
    assert!(captured[0].headers.get(header::HOST).is_none());
    assert!(captured[0].headers.get("x-amz-date").is_none());
    assert_eq!(captured[0].headers.get("x-safe").unwrap(), "kept");
    drop(captured);

    let local = Rc::new(LocalCapture::default());
    block_on(
        LocalBedrockSignerTransport::new(local.clone())
            .execute(logical_request(headers), CancellationToken::new()),
    )
    .expect("Local build stage");
    let captured = local.requests.borrow();
    assert!(captured[0].headers.get(header::AUTHORIZATION).is_none());
    assert!(captured[0].headers.get(header::HOST).is_none());
    assert!(captured[0].headers.get("x-amz-date").is_none());
    assert_eq!(captured[0].headers.get("x-safe").unwrap(), "kept");
}

/// Architecture v2 part 2 §2.6 and §6.1; pinned Pi basis:
/// `amazon-bedrock.ts` bearer-token resolution and
/// `bedrock-converse-stream.ts` `httpBearerAuth` selection.
#[test]
fn bedrock_bearer_auth_is_reasserted_before_signing() {
    let send = Arc::new(SendCapture::default());
    let mut request = logical_request(HeaderMap::new());
    request.auth_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer resolved-token"),
    );
    block_on(BedrockSignerTransport::new(send.clone()).execute(request, CancellationToken::new()))
        .expect("bearer build stage");
    assert_eq!(
        send.requests.lock().expect("capture lock")[0]
            .headers
            .get(header::AUTHORIZATION),
        Some(&HeaderValue::from_static("Bearer resolved-token"))
    );
}

struct RawResponseTransport;

impl BedrockSigner for RawResponseTransport {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>> {
        Box::pin(async {
            let mut headers = HeaderMap::new();
            headers.insert("x-gateway-trace", HeaderValue::from_static("raw-value"));
            Ok(BedrockSignerResponse::empty(200, headers))
        })
    }
}

struct LocalRawResponseTransport;

impl LocalBedrockSigner for LocalRawResponseTransport {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalBedrockSignerResponse, BedrockSignerError>> {
        Box::pin(async {
            let mut headers = HeaderMap::new();
            headers.insert("x-gateway-trace", HeaderValue::from_static("raw-local"));
            Ok(LocalBedrockSignerResponse::empty(200, headers))
        })
    }
}

struct BodyDiagnosticSigner;

impl BedrockSigner for BodyDiagnosticSigner {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>> {
        Box::pin(async {
            let diagnostic = agentprism_ai::AssistantMessageDiagnostic {
                schema_version: agentprism_ai::ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
                kind: "bedrock_transport_recovery".to_owned(),
                timestamp: Timestamp::default(),
                error: None,
                details: [("attempt".to_owned(), json!(1))].into_iter().collect(),
            };
            Ok(BedrockSignerResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                body: Box::pin(futures_util::stream::once(async move {
                    Err(BedrockSignerError::from(
                        TransportError::new("stream", "body failed").with_diagnostic(diagnostic),
                    ))
                })),
            })
        })
    }
}

struct LocalBodyDiagnosticSigner;

impl LocalBedrockSigner for LocalBodyDiagnosticSigner {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalBedrockSignerResponse, BedrockSignerError>> {
        Box::pin(async {
            let diagnostic = agentprism_ai::AssistantMessageDiagnostic {
                schema_version: agentprism_ai::ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
                kind: "bedrock_transport_recovery".to_owned(),
                timestamp: Timestamp::default(),
                error: None,
                details: [("attempt".to_owned(), json!(1))].into_iter().collect(),
            };
            Ok(LocalBedrockSignerResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                body: Box::pin(futures_util::stream::once(async move {
                    Err(BedrockSignerError::from(
                        TransportError::new("stream", "body failed").with_diagnostic(diagnostic),
                    ))
                })),
            })
        })
    }
}

#[derive(Clone, Copy)]
enum TypedFailurePoint {
    Establishment,
    Body,
}

struct TypedFailureSigner {
    point: TypedFailurePoint,
    failure: BedrockProviderFailure,
}

impl BedrockSigner for TypedFailureSigner {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>> {
        let failure = self.failure.clone();
        Box::pin(async move {
            match self.point {
                TypedFailurePoint::Establishment => Err(failure.into()),
                TypedFailurePoint::Body => Ok(BedrockSignerResponse {
                    status: 200,
                    headers: HeaderMap::new(),
                    diagnostics: Vec::new(),
                    notify_observers: true,
                    body: Box::pin(futures_util::stream::once(
                        async move { Err(failure.into()) },
                    )),
                }),
            }
        })
    }
}

struct LocalTypedFailureSigner {
    point: TypedFailurePoint,
    failure: BedrockProviderFailure,
}

impl LocalBedrockSigner for LocalTypedFailureSigner {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalBedrockSignerResponse, BedrockSignerError>> {
        let failure = self.failure.clone();
        Box::pin(async move {
            match self.point {
                TypedFailurePoint::Establishment => Err(failure.into()),
                TypedFailurePoint::Body => Ok(LocalBedrockSignerResponse {
                    status: 200,
                    headers: HeaderMap::new(),
                    diagnostics: Vec::new(),
                    notify_observers: true,
                    body: Box::pin(futures_util::stream::once(
                        async move { Err(failure.into()) },
                    )),
                }),
            }
        })
    }
}

#[derive(Default)]
struct SendObserver {
    value: Mutex<Option<HeaderValue>>,
}

impl ResponseObserver for SendObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        *self.value.lock().expect("observer lock") =
            response.headers.get("x-gateway-trace").cloned();
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct LocalObserver {
    seen: Cell<bool>,
}

impl LocalResponseObserver for LocalObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        self.seen.set(
            response.headers.get("x-gateway-trace") == Some(&HeaderValue::from_static("raw-local")),
        );
        Box::pin(async { Ok(()) })
    }
}

fn resolved_request(model: ModelDescriptor) -> ResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    ResolvedApiRequest {
        model,
        context: one_user_context(),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: Default::default(),
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: ApiId::new(BedrockConverseStream::API_ID),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_resolved_request(model: ModelDescriptor) -> LocalResolvedApiRequest {
    let send = resolved_request(model);
    LocalResolvedApiRequest {
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
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: send.retry_policy,
        timeout: send.timeout,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `bedrock-response-headers.test.ts` and `bedrock-converse-stream.ts:486-508`.
#[test]
fn bedrock_response_observer_receives_raw_headers() {
    let model = model("us.anthropic.claude-opus-4-8");
    let observer = Arc::new(SendObserver::default());
    let mut request = resolved_request(model.clone());
    request.response_observers = Arc::from([observer.clone() as Arc<dyn ResponseObserver>]);
    let api = bedrock_converse_stream_api(Arc::new(RawResponseTransport));
    let _stream = block_on(api.stream(request, CancellationToken::new())).expect("Send stream");
    assert_eq!(
        observer.value.lock().expect("observer lock").as_ref(),
        Some(&HeaderValue::from_static("raw-value"))
    );

    let observer = Rc::new(LocalObserver::default());
    let mut request = local_resolved_request(model);
    request.response_observers = Rc::from([observer.clone() as Rc<dyn LocalResponseObserver>]);
    let api = local_bedrock_converse_stream_api(Rc::new(LocalRawResponseTransport));
    let _stream = block_on(api.stream(request, CancellationToken::new())).expect("Local stream");
    assert!(observer.seen.get());
}

/// Architecture v2 part 2 §2.1 and §9.2; body-stream recovery diagnostics
/// remain in-band in both executor families.
#[test]
fn bedrock_body_transport_diagnostic_is_committed_send_and_local() {
    let model = model("us.anthropic.claude-opus-4-8");
    let api = bedrock_converse_stream_api(Arc::new(BodyDiagnosticSigner));
    let mut stream =
        block_on(api.stream(resolved_request(model.clone()), CancellationToken::new()))
            .expect("Send body-error stream");
    let send_events = block_on(async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    });
    assert!(
        terminal(&send_events)
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == "bedrock_transport_recovery")
    );

    let api = local_bedrock_converse_stream_api(Rc::new(LocalBodyDiagnosticSigner));
    let mut stream = block_on(api.stream(local_resolved_request(model), CancellationToken::new()))
        .expect("Local body-error stream");
    let local_events = block_on(async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    });
    assert!(
        terminal(&local_events)
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == "bedrock_transport_recovery")
    );
}

const BEDROCK_FIXTURE_CASES: [&str; 28] = [
    "text-only",
    "system-developer-prompt",
    "images",
    "thinking-disabled",
    "reasoning-minimal",
    "reasoning-low",
    "reasoning-medium",
    "reasoning-high",
    "reasoning-xhigh",
    "reasoning-max",
    "signed-thinking-replay",
    "redacted-encrypted-reasoning-replay",
    "one-tool-call",
    "multiple-tool-calls",
    "tool-results",
    "tool-result-images",
    "orphan-result-repair",
    "cache-disabled",
    "cache-short",
    "cache-long",
    "sampling-defaults-and-overrides",
    "max-output-clamp",
    "strict-tool-schema",
    "provider-model-headers",
    "session-affinity",
    "api-specific-compat-flags",
    "cross-provider-handoff",
    "failed-turn-omission",
];

fn bedrock_fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/bedrock-converse-stream")
}

fn fixture_model(case: &str) -> ModelDescriptor {
    let mut model = model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    model.common.display_name = "Fixture Bedrock Claude Sonnet 4.5".to_owned();
    model.common.base_url = Url::parse("http://127.0.0.1:4567").expect("fixture endpoint");
    model.common.limits.context_window = 128_000;
    model.common.limits.max_output_tokens = 8_192;
    if case == "max-output-clamp" {
        model.common.limits.context_window = 4_200;
        model.common.limits.max_output_tokens = 2_048;
    }
    if case == "provider-model-headers" {
        model
            .common
            .headers
            .insert("x-fixture-model".to_owned(), Some("model-value".to_owned()));
    }
    if case == "api-specific-compat-flags"
        && let agentprism_ai::ApiModelConfig::BedrockConverse(config) = &mut model.api
    {
        config.compat.supports_strict_mode = Some(false);
    }
    model
}

fn fixture_tool(strict: bool) -> ToolSpec {
    ToolSpec {
        schema_version: 1,
        name: "read_file".to_owned(),
        description: "Read one UTF-8 file.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative path."}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        constrained_sampling: strict.then_some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: JsonSchemaStrictMode::Require,
            },
        )),
    }
}

fn fixture_tool_result(call_id: &str, image: bool) -> Message {
    let mut content = vec![ToolResultContent::Text {
        id: ContentBlockId::new(format!("{call_id}-result-text")),
        text: if image {
            "fixture image".to_owned()
        } else {
            "fixture file contents".to_owned()
        },
    }];
    if image {
        content.push(ToolResultContent::Image {
            id: ContentBlockId::new(format!("{call_id}-result-image")),
            data: PNG_1X1.to_owned(),
            mime_type: "image/png".to_owned(),
        });
    }
    Message::ToolResult(ToolResultMessage {
        id: MessageId::new(format!("{call_id}-result")),
        tool_call_id: ToolCallId::new(call_id),
        tool_name: "read_file".to_owned(),
        content,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    })
}

#[derive(Clone, Copy)]
enum FixtureResponseKind {
    Text,
    Tool,
    MultipleTools,
    SignedReasoning,
    RedactedReasoning,
}

fn fixture_response(
    model: &ModelDescriptor,
    kind: FixtureResponseKind,
) -> agentprism_ai::AssistantMessage {
    let message_id = match kind {
        FixtureResponseKind::Text => "fixture-text-response",
        FixtureResponseKind::Tool => "fixture-tool-response",
        FixtureResponseKind::MultipleTools => "fixture-multiple-tools-response",
        FixtureResponseKind::SignedReasoning => "fixture-signed-response",
        FixtureResponseKind::RedactedReasoning => "fixture-redacted-response",
    };
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new(message_id),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event("messageStart", &json!({"role":"assistant"}));
    match kind {
        FixtureResponseKind::Text => {
            let _ = decoder.push_event(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"text":"fixture response turn 1"}}),
            );
            let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
        }
        FixtureResponseKind::Tool
        | FixtureResponseKind::MultipleTools
        | FixtureResponseKind::SignedReasoning
        | FixtureResponseKind::RedactedReasoning => {
            let mut tool_index = 0;
            if matches!(kind, FixtureResponseKind::SignedReasoning) {
                let _ = decoder.push_event(
                    "contentBlockDelta",
                    &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Inspect the requested fixture."}}}),
                );
                let _ = decoder.push_event(
                    "contentBlockDelta",
                    &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"signed-fixture-reasoning"}}}),
                );
                let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
                tool_index = 1;
            } else if matches!(kind, FixtureResponseKind::RedactedReasoning) {
                let _ = decoder.push_event(
                    "contentBlockDelta",
                    &json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":[1,2,3,255]}}}),
                );
                let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":0}));
                tool_index = 1;
            }
            let paths: &[(&str, &str)] = if matches!(kind, FixtureResponseKind::MultipleTools) {
                &[
                    (FIXTURE_TOOL_CALL_1, "Cargo.toml"),
                    (FIXTURE_TOOL_CALL_2, "README.md"),
                ]
            } else {
                &[(FIXTURE_TOOL_CALL_1, "Cargo.toml")]
            };
            for (offset, (call_id, path)) in paths.iter().enumerate() {
                let index = tool_index + u32::try_from(offset).expect("fixture tool index");
                let _ = decoder.push_event(
                    "contentBlockStart",
                    &json!({"contentBlockIndex":index,"start":{"toolUse":{"toolUseId":call_id,"name":"read_file"}}}),
                );
                let _ = decoder.push_event(
                    "contentBlockDelta",
                    &json!({"contentBlockIndex":index,"delta":{"toolUse":{"input":format!(r#"{{"path":"{path}"}}"#)}}}),
                );
                let _ = decoder.push_event("contentBlockStop", &json!({"contentBlockIndex":index}));
            }
        }
    }
    let stop = if matches!(kind, FixtureResponseKind::Text) {
        "end_turn"
    } else {
        "tool_use"
    };
    let _ = decoder.push_event("messageStop", &json!({"stopReason":stop}));
    let _ = decoder.push_event(
        "metadata",
        &json!({"usage":{"inputTokens":12,"outputTokens":8,"totalTokens":20}}),
    );
    let message = terminal(&decoder.finish());
    serde_json::from_str(&serde_json::to_string(&message).expect("persist fixture response"))
        .expect("restore fixture response")
}

fn failed_fixture_response(model: &ModelDescriptor) -> agentprism_ai::AssistantMessage {
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("failed-fixture-response"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event(
        "contentBlockDelta",
        &json!({"contentBlockIndex":0,"delta":{"text":"partial secret-free text"}}),
    );
    terminal(&decoder.fail_transport("fixture", "fixture transport failure"))
}

fn fixture_context(case: &str, model: &ModelDescriptor) -> Context {
    let mut context = Context::new(match case {
        "system-developer-prompt" => Some("Fixture system instruction.".to_owned()),
        "cache-disabled" | "cache-short" | "cache-long" => Some("Cache fixture.".to_owned()),
        _ => None,
    });
    match case {
        "images" => context.messages.push(Message::User(UserMessage {
            id: MessageId::new("fixture-image-user"),
            content: vec![
                ContentBlock::Text {
                    id: ContentBlockId::new("fixture-image-text"),
                    text: "Describe this fixture image.".to_owned(),
                },
                ContentBlock::Image {
                    id: ContentBlockId::new("fixture-image"),
                    data: PNG_1X1.to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
        })),
        "one-tool-call" | "strict-tool-schema" => context
            .messages
            .push(user("fixture-user", "Read Cargo.toml.")),
        "multiple-tool-calls" => context
            .messages
            .push(user("fixture-user", "Read Cargo.toml and README.md.")),
        "signed-thinking-replay" => context
            .messages
            .push(user("fixture-user", "Reason, then read Cargo.toml.")),
        "redacted-encrypted-reasoning-replay" => context.messages.push(user(
            "fixture-user",
            "Privately reason, then read Cargo.toml.",
        )),
        "tool-results" | "api-specific-compat-flags" => {
            context
                .messages
                .push(user("fixture-user-1", "Read Cargo.toml."));
            context.messages.push(Message::Assistant(fixture_response(
                model,
                FixtureResponseKind::Tool,
            )));
            context
                .messages
                .push(fixture_tool_result(FIXTURE_TOOL_CALL_1, false));
            context
                .messages
                .push(user("fixture-user-2", "Summarize the result."));
        }
        "tool-result-images" => {
            context
                .messages
                .push(user("fixture-user-1", "Read an image."));
            let mut assistant = fixture_response(model, FixtureResponseKind::Tool);
            let ContentBlock::ToolCall { call, .. } = &mut assistant.content[0] else {
                panic!("fixture tool response")
            };
            call.arguments = json!({"path":"fixture.png"});
            context.messages.push(Message::Assistant(assistant));
            context
                .messages
                .push(fixture_tool_result(FIXTURE_TOOL_CALL_1, true));
            context
                .messages
                .push(user("fixture-user-2", "Describe it."));
        }
        "orphan-result-repair" => {
            context
                .messages
                .push(user("fixture-user-1", "Read Cargo.toml."));
            context.messages.push(Message::Assistant(fixture_response(
                model,
                FixtureResponseKind::Tool,
            )));
            context
                .messages
                .push(user("fixture-user-2", "Continue without the result."));
        }
        "cross-provider-handoff" => {
            context
                .messages
                .push(user("fixture-user-1", "Think about the fixture."));
            let mut assistant = terminal(&decode_reasoning(
                model,
                &[
                    json!({"text":"Foreign visible reasoning."}),
                    json!({"signature":"foreign-signature"}),
                ],
                true,
            ));
            assistant.provider = ProviderId::new("foreign-provider");
            assistant.api = ApiId::new("foreign-api");
            assistant.requested_model = ModelId::new("foreign-model");
            assistant.content.push(ContentBlock::Text {
                id: ContentBlockId::new("foreign-answer"),
                text: "Foreign answer.".to_owned(),
            });
            context.messages.push(Message::Assistant(assistant));
            context
                .messages
                .push(user("fixture-user-2", "Continue on the target model."));
        }
        "failed-turn-omission" => {
            context
                .messages
                .push(user("fixture-user-1", "First attempt."));
            context
                .messages
                .push(Message::Assistant(failed_fixture_response(model)));
            context
                .messages
                .push(user("fixture-user-2", "Retry cleanly."));
        }
        _ => context
            .messages
            .push(user("fixture-user", "Return a concise fixture response.")),
    }
    if matches!(
        case,
        "signed-thinking-replay"
            | "redacted-encrypted-reasoning-replay"
            | "one-tool-call"
            | "multiple-tool-calls"
            | "tool-results"
            | "tool-result-images"
            | "orphan-result-repair"
            | "strict-tool-schema"
            | "api-specific-compat-flags"
    ) {
        context
            .tools
            .push(fixture_tool(case == "strict-tool-schema"));
    }
    context
}

enum FixtureCall {
    Simple(SimpleGenerationOptions),
    Full(BedrockOptions),
}

fn fixture_call(case: &str) -> FixtureCall {
    let reasoning = match case {
        "reasoning-minimal" => Some(ReasoningLevel::Minimal),
        "reasoning-low" | "signed-thinking-replay" | "redacted-encrypted-reasoning-replay" => {
            Some(ReasoningLevel::Low)
        }
        "reasoning-medium" => Some(ReasoningLevel::Medium),
        "reasoning-high" => Some(ReasoningLevel::High),
        "reasoning-xhigh" => Some(ReasoningLevel::Xhigh),
        "reasoning-max" => Some(ReasoningLevel::Max),
        _ => None,
    };
    if case == "thinking-disabled"
        || case.starts_with("reasoning-")
        || matches!(
            case,
            "signed-thinking-replay"
                | "redacted-encrypted-reasoning-replay"
                | "sampling-defaults-and-overrides"
                | "max-output-clamp"
        )
    {
        let mut simple = SimpleGenerationOptions {
            reasoning,
            ..Default::default()
        };
        if case.starts_with("reasoning-") {
            simple.max_output_tokens = Some(4_096);
        } else if matches!(
            case,
            "signed-thinking-replay" | "redacted-encrypted-reasoning-replay"
        ) {
            simple.max_output_tokens = Some(1_024);
        } else if case == "sampling-defaults-and-overrides" {
            simple.temperature = Some(0.0);
        } else if case == "max-output-clamp" {
            simple.max_output_tokens = Some(9_000);
        }
        FixtureCall::Simple(simple)
    } else {
        let cache_retention = match case {
            "cache-disabled" => CacheRetention::None,
            "cache-long" => CacheRetention::Long,
            _ => CacheRetention::Short,
        };
        FixtureCall::Full(BedrockOptions {
            cache_retention: Some(cache_retention),
            ..Default::default()
        })
    }
}

fn fixture_kind(case: &str) -> FixtureResponseKind {
    match case {
        "one-tool-call" => FixtureResponseKind::Tool,
        "multiple-tool-calls" => FixtureResponseKind::MultipleTools,
        "signed-thinking-replay" => FixtureResponseKind::SignedReasoning,
        "redacted-encrypted-reasoning-replay" => FixtureResponseKind::RedactedReasoning,
        _ => FixtureResponseKind::Text,
    }
}

fn fixture_http_body(
    model: &ModelDescriptor,
    context: Context,
    call: &FixtureCall,
    case: &str,
) -> (Vec<u8>, HeaderMap) {
    let capture = Arc::new(SendCapture::default());
    let mut request = resolved_request(model.clone());
    request.context = context;
    request.endpoint = model.common.base_url.clone();
    for (name, value) in &model.common.headers {
        if let Some(value) = value {
            request.headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("fixture model header"),
                HeaderValue::from_str(value).expect("fixture model header value"),
            );
        }
    }
    if case == "provider-model-headers" {
        request.headers.insert(
            "x-fixture-request",
            HeaderValue::from_static("request-value"),
        );
    }
    match call {
        FixtureCall::Simple(options) => request.options = options.clone(),
        FixtureCall::Full(options) => {
            request.full_options = Some(ErasedApiFullOptions::new::<BedrockConverseStream>(
                options.clone(),
            ));
        }
    }
    let api = bedrock_converse_stream_api(capture.clone());
    let _stream = block_on(api.stream(request, CancellationToken::new())).expect("fixture request");
    let captured = capture.requests.lock().expect("fixture capture lock");
    (captured[0].body.clone(), captured[0].headers.clone())
}

fn append_fixture_turn_two(
    context: &mut Context,
    assistant: agentprism_ai::AssistantMessage,
    kind: FixtureResponseKind,
) {
    context.messages.push(Message::Assistant(assistant));
    match kind {
        FixtureResponseKind::Tool
        | FixtureResponseKind::SignedReasoning
        | FixtureResponseKind::RedactedReasoning => context
            .messages
            .push(fixture_tool_result(FIXTURE_TOOL_CALL_1, false)),
        FixtureResponseKind::MultipleTools => {
            context
                .messages
                .push(fixture_tool_result(FIXTURE_TOOL_CALL_1, false));
            context
                .messages
                .push(fixture_tool_result(FIXTURE_TOOL_CALL_2, false));
        }
        FixtureResponseKind::Text => context.messages.push(user(
            "fixture-turn-two-user",
            "Deterministic turn-two follow-up.",
        )),
    }
}

fn decode_fixture_response_bytes(
    model: &ModelDescriptor,
    bytes: &[u8],
) -> agentprism_ai::AssistantMessage {
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("captured-fixture-response"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    });
    let mut events = decoder.take_events();
    events.extend(decoder.push(bytes));
    events.extend(decoder.finish());
    let message = terminal(&events);
    serde_json::from_str(&serde_json::to_string(&message).expect("persist captured fixture"))
        .expect("restore captured fixture")
}

fn assert_bedrock_fixture_inventory() {
    let root = bedrock_fixture_root();
    let mut discovered = fs::read_dir(&root)
        .expect("Bedrock fixture directory")
        .map(|entry| {
            entry
                .expect("Bedrock fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    discovered.sort();
    let mut expected_cases = BEDROCK_FIXTURE_CASES.map(str::to_owned).to_vec();
    expected_cases.sort();
    assert_eq!(discovered, expected_cases, "complete §10.8 Bedrock corpus");
    for case in BEDROCK_FIXTURE_CASES {
        let directory = root.join(case);
        for artifact in [
            "canonical.json",
            "metadata.json",
            "request-turn-1.body.json",
            "request-turn-1.headers.json",
            "request-turn-2.body.json",
            "request-turn-2.headers.json",
            "response-turn-1.sse",
        ] {
            assert!(directory.join(artifact).is_file(), "{case}/{artifact}");
        }
        let canonical: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.join("canonical.json")).expect("canonical fixture"),
        )
        .expect("canonical JSON");
        assert_eq!(canonical["schemaVersion"], 1);
        assert_eq!(canonical["family"], "bedrock-converse-stream");
        assert_eq!(canonical["case"], case);
        assert_eq!(
            canonical["piCommit"],
            "8fa7eebd235355522c8104166b4f1f959b4e2f10"
        );
    }
}

/// Architecture v2 part 2 §10.8 fixture inventory and provenance.
#[test]
fn fixture_corpus_bedrock_converse_stream_cases_are_complete_and_canonical() {
    assert_bedrock_fixture_inventory();
}

/// Architecture v2 part 2 §10.8; pinned Pi basis: the complete 28-case
/// `bedrock-converse-stream.ts` two-turn fixture corpus captured at the pin.
#[test]
fn wire_bedrock_converse_stream_pi_exact() {
    let root = bedrock_fixture_root();
    assert_bedrock_fixture_inventory();

    for case in BEDROCK_FIXTURE_CASES {
        let directory = root.join(case);
        let model = fixture_model(case);
        let mut context = fixture_context(case, &model);
        let call = fixture_call(case);
        let (turn_one, headers) = fixture_http_body(&model, context.clone(), &call, case);
        assert_eq!(
            turn_one,
            fs::read(directory.join("request-turn-1.body.json")).expect("turn-one fixture"),
            "turn one: {case}"
        );
        if case == "provider-model-headers" {
            assert_eq!(headers.get("x-fixture-model").unwrap(), "model-value");
            assert_eq!(headers.get("x-fixture-request").unwrap(), "request-value");
        }

        let assistant = decode_fixture_response_bytes(
            &model,
            &fs::read(directory.join("response-turn-1.sse")).expect("captured response fixture"),
        );
        append_fixture_turn_two(&mut context, assistant, fixture_kind(case));
        let (turn_two, _) = fixture_http_body(&model, context, &call, case);
        assert_eq!(
            turn_two,
            fs::read(directory.join("request-turn-2.body.json")).expect("turn-two fixture"),
            "turn two: {case}"
        );
    }
}

/// Architecture v2 part 2 §1.7 and §10.8; pinned Pi basis:
/// `bedrock-redacted-reasoning.test.ts` turn-two body fixture.
#[test]
fn bedrock_redacted_reasoning_turn_two_pi_exact() {
    let model = model("openai.gpt-5.6-terra");
    let assistant = terminal(&decode_reasoning(
        &model,
        &[
            json!({"redactedContent":[1,2]}),
            json!({"redactedContent":[3,255]}),
        ],
        true,
    ));
    let assistant: agentprism_ai::AssistantMessage =
        serde_json::from_str(&serde_json::to_string(&assistant).expect("persist assistant"))
            .expect("restore assistant");
    let mut context = one_user_context();
    context.messages.push(Message::Assistant(assistant));
    context.messages.push(user("user-2", "continue"));
    assert_eq!(
        final_http_body(encode_command(
            &model,
            &context,
            &BedrockOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        )),
        br#"{"messages":[{"role":"user","content":[{"text":"hello"}]},{"role":"assistant","content":[{"reasoningContent":{"redactedContent":"AQID/w=="}}]},{"role":"user","content":[{"text":"continue"}]}],"inferenceConfig":{}}"#
    );
}

/// Architecture v2 part 2 §5.1 and §10.8; pinned Pi basis:
/// `amazon-bedrock.models.ts` and `bedrock-models.test.ts`.
#[test]
fn bedrock_catalog_matches_pinned_pi() {
    let models = bedrock_models();
    assert_eq!(models.len(), 114);
    assert!(
        models
            .iter()
            .any(|model| model.common.model_ref.model.as_str() == "global.anthropic.claude-opus-5")
    );
    assert!(
        models
            .iter()
            .all(|model| model.common.model_ref.model.as_str() != "anthropic.claude-opus-5")
    );
    assert_eq!(
        model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0")
            .common
            .base_url
            .as_str(),
        "https://bedrock-runtime.eu-central-1.amazonaws.com/"
    );
}

fn put_bedrock_credential(store: &InMemoryCredentialStore, credential: ApiKeyCredential) {
    block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new("amazon-bedrock"), CancellationToken::new())
            .await
            .expect("credential lease");
        lease.replace(Some(Credential::ApiKey(credential)));
        lease.commit().await.expect("credential commit");
    });
}

fn put_local_bedrock_credential(
    store: &LocalInMemoryCredentialStore,
    credential: ApiKeyCredential,
) {
    block_on(async {
        let mut lease = LocalCredentialStore::acquire_lease(
            store,
            ProviderId::new("amazon-bedrock"),
            CancellationToken::new(),
        )
        .await
        .expect("local credential lease");
        lease.replace(Some(Credential::ApiKey(credential)));
        lease.commit().await.expect("local credential commit");
    });
}

fn unidentified_application_profile() -> ModelDescriptor {
    let mut model = model("us.anthropic.claude-opus-4-8");
    model.common.model_ref.model = ModelId::new(
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/cache-profile",
    );
    model.common.display_name = "Application inference profile".to_owned();
    model
}

fn context_with_system_prompt() -> Context {
    let mut context = one_user_context();
    context.system_prompt = Some("Cache this system prompt.".to_owned());
    context
}

fn send_body_with_resolved_auth(model: ModelDescriptor, auth: &ResolvedAuth) -> Vec<u8> {
    let capture = Arc::new(SendCapture::default());
    let mut request = resolved_request(model);
    request.context = context_with_system_prompt();
    request.headers = auth.headers.clone();
    request.auth_headers = resolved_transport_headers(auth);
    let api = bedrock_converse_stream_api(capture.clone());
    let _stream = block_on(api.stream(request, CancellationToken::new()))
        .expect("Send provider-environment request");
    capture.requests.lock().expect("capture lock")[0]
        .body
        .clone()
}

fn local_body_with_resolved_auth(model: ModelDescriptor, auth: &ResolvedAuth) -> Vec<u8> {
    let capture = Rc::new(LocalCapture::default());
    let mut request = local_resolved_request(model);
    request.context = context_with_system_prompt();
    request.headers = auth.headers.clone();
    request.auth_headers = resolved_transport_headers(auth);
    let api = local_bedrock_converse_stream_api(capture.clone());
    let _stream = block_on(api.stream(request, CancellationToken::new()))
        .expect("Local provider-environment request");
    capture.requests.borrow()[0].body.clone()
}

fn cache_environment() -> BTreeMap<String, String> {
    [
        ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
        (
            "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
            "cache-test-token".to_owned(),
        ),
        ("AWS_BEDROCK_FORCE_CACHE".to_owned(), "1".to_owned()),
        ("PI_CACHE_RETENTION".to_owned(), "long".to_owned()),
    ]
    .into_iter()
    .collect()
}

fn assert_forced_long_cache_body(body: &[u8]) {
    let body = std::str::from_utf8(body).expect("Bedrock JSON body");
    assert_eq!(body.matches(r#""cachePoint""#).count(), 2);
    assert_eq!(body.matches(r#""ttl":"1h""#).count(), 2);
}

/// Architecture v2 part 2 §2.6 and §10.8; pinned Pi basis:
/// `bedrock-converse-stream.ts:792-840` and `provider-env.ts`. Request-scoped
/// provider environment wins over ambient values, and both executor families
/// apply the same cache behavior without reading the process environment.
#[test]
fn bedrock_provider_environment_cache_controls_send_and_local() {
    let send_registration = bedrock_provider(Arc::new(SendCapture::default()))
        .expect("Send Bedrock provider registration");
    for request_scoped in [true, false] {
        let model = unidentified_application_profile();
        let mut request =
            ResolveAuthRequest::isolated(send_registration.descriptor.clone(), Some(model.clone()));
        if request_scoped {
            request.overrides = AuthResolutionOverrides {
                environment: cache_environment(),
                ..Default::default()
            };
            request.auth_context = Arc::new(MapAuthContext::new(
                [
                    ("AWS_BEDROCK_FORCE_CACHE".to_owned(), "0".to_owned()),
                    ("PI_CACHE_RETENTION".to_owned(), "short".to_owned()),
                ]
                .into_iter()
                .collect(),
                [],
            ));
        } else {
            request.auth_context = Arc::new(MapAuthContext::new(cache_environment(), []));
        }
        let resolved = block_on(
            send_registration
                .auth
                .resolve(request, CancellationToken::new()),
        )
        .expect("Send provider-environment resolution")
        .expect("skip-auth resolution");
        assert_forced_long_cache_body(&send_body_with_resolved_auth(model, &resolved));
    }

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock provider registration");
    for request_scoped in [true, false] {
        let model = unidentified_application_profile();
        let mut request = LocalResolveAuthRequest::isolated(
            local_registration.descriptor.clone(),
            Some(model.clone()),
        );
        if request_scoped {
            request.overrides = AuthResolutionOverrides {
                environment: cache_environment(),
                ..Default::default()
            };
            request.auth_context = Rc::new(MapAuthContext::new(
                [
                    ("AWS_BEDROCK_FORCE_CACHE".to_owned(), "0".to_owned()),
                    ("PI_CACHE_RETENTION".to_owned(), "short".to_owned()),
                ]
                .into_iter()
                .collect(),
                [],
            ));
        } else {
            request.auth_context = Rc::new(MapAuthContext::new(cache_environment(), []));
        }
        let resolved = block_on(
            local_registration
                .auth
                .resolve(request, CancellationToken::new()),
        )
        .expect("Local provider-environment resolution")
        .expect("skip-auth resolution");
        assert_forced_long_cache_body(&local_body_with_resolved_auth(model, &resolved));
    }

    let mut explicit = BedrockOptions {
        cache_retention: Some(CacheRetention::None),
        ..Default::default()
    };
    explicit.provider_environment.long_cache_retention = true;
    explicit.provider_environment.force_prompt_caching = true;
    let body = encode_command(
        &unidentified_application_profile(),
        &context_with_system_prompt(),
        &explicit,
    );
    assert!(!body.contains("cachePoint"));
}

fn govcloud_models_environment() -> BTreeMap<String, String> {
    [
        ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
        (
            "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
            "govcloud-test-token".to_owned(),
        ),
        ("AWS_REGION".to_owned(), "us-gov-west-1".to_owned()),
    ]
    .into_iter()
    .collect()
}

fn govcloud_reasoning_request() -> ModelRequest {
    ModelRequest {
        model: model("global.anthropic.claude-opus-5").common.model_ref,
        context: one_user_context(),
        options: SimpleGenerationOptions {
            reasoning: Some(ReasoningLevel::High),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    }
}

fn assert_govcloud_models_capture(request: &HttpRequest, config: &BedrockSigningConfig) {
    let body = std::str::from_utf8(&request.body).expect("Bedrock JSON body");
    assert!(body.contains(r#""thinking":{"type":"adaptive"}"#));
    assert!(!body.contains("display"));
    assert_eq!(config.region.as_deref(), Some("us-gov-west-1"));
}

/// Architecture v2 part 2 §1.7, §2.6, and §9.2; pinned Pi basis:
/// `bedrock-thinking-payload.test.ts`, `provider-env.ts`, and
/// `bedrock-converse-stream.ts:1156,1206-1227`. The request-scoped AWS region
/// selected by auth must shape the command before either Models trait family
/// hands it to the signer.
#[test]
fn bedrock_models_environment_region_shapes_govcloud_send_and_local_pi_exact() {
    let send_capture = Arc::new(SendCapture::default());
    let send_models = Models::builder()
        .auth_context(Arc::new(MapAuthContext::new(
            govcloud_models_environment(),
            [],
        )))
        .provider(bedrock_provider(send_capture.clone()).expect("Send Bedrock registration"))
        .build()
        .expect("Send Models");
    let _stream =
        block_on(send_models.stream_simple(govcloud_reasoning_request(), CancellationToken::new()))
            .expect("Send Models Bedrock stream");
    assert_govcloud_models_capture(
        &send_capture.requests.lock().expect("capture lock")[0],
        &send_capture.configs.lock().expect("capture lock")[0],
    );

    let local_capture = Rc::new(LocalCapture::default());
    let local_models = LocalModels::builder()
        .auth_context(Rc::new(MapAuthContext::new(
            govcloud_models_environment(),
            [],
        )))
        .provider(
            local_bedrock_provider(local_capture.clone()).expect("Local Bedrock registration"),
        )
        .build()
        .expect("Local Models");
    let _stream = block_on(
        local_models.stream_simple(govcloud_reasoning_request(), CancellationToken::new()),
    )
    .expect("Local Models Bedrock stream");
    assert_govcloud_models_capture(
        &local_capture.requests.borrow()[0],
        &local_capture.configs.borrow()[0],
    );
}

fn collision_request(value: &'static str) -> ModelRequest {
    ModelRequest {
        model: model("us.anthropic.claude-opus-4-8").common.model_ref,
        context: one_user_context(),
        options: SimpleGenerationOptions {
            headers: [(
                "x-pi-bedrock-signing-config".to_owned(),
                Some(value.to_owned()),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    }
}

fn skip_auth_context() -> MapAuthContext {
    MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "header-test-token".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    )
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `bedrock-custom-headers.test.ts` and
/// `bedrock-converse-stream.ts:440-473`. The auth carrier is suppressed only
/// when it remains the original private value; explicit and transform overlays
/// using the same non-reserved name are forwarded into signing.
#[test]
fn bedrock_private_signer_header_collision_preserves_later_overlays_send_and_local() {
    let send_explicit = Arc::new(SendCapture::default());
    let models = Models::builder()
        .auth_context(Arc::new(skip_auth_context()))
        .provider(bedrock_provider(send_explicit.clone()).expect("Send Bedrock registration"))
        .build()
        .expect("Send Models");
    let _stream = block_on(models.stream_simple(
        collision_request("caller-explicit-send"),
        CancellationToken::new(),
    ))
    .expect("Send explicit collision");
    assert_eq!(
        send_explicit.requests.lock().expect("capture lock")[0]
            .headers
            .get("x-pi-bedrock-signing-config"),
        Some(&HeaderValue::from_static("caller-explicit-send"))
    );

    let send_transform = Arc::new(SendCapture::default());
    let models = Models::builder()
        .auth_context(Arc::new(skip_auth_context()))
        .provider(bedrock_provider(send_transform.clone()).expect("Send Bedrock registration"))
        .header_transform(Arc::new(PrivateHeaderTransform("caller-transform-send")))
        .build()
        .expect("Send Models with transform");
    let _stream = block_on(models.stream_simple(
        collision_request("caller-explicit-before-transform"),
        CancellationToken::new(),
    ))
    .expect("Send transform collision");
    assert_eq!(
        send_transform.requests.lock().expect("capture lock")[0]
            .headers
            .get("x-pi-bedrock-signing-config"),
        Some(&HeaderValue::from_static("caller-transform-send"))
    );

    let send_exact_bytes = Arc::new(SendCapture::default());
    let send_registration =
        bedrock_provider(send_exact_bytes.clone()).expect("Send Bedrock registration");
    let mut resolve = ResolveAuthRequest::isolated(
        send_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    resolve.auth_context = Arc::new(skip_auth_context());
    let resolved = block_on(
        send_registration
            .auth
            .resolve(resolve, CancellationToken::new()),
    )
    .expect("resolve Send private carrier")
    .expect("Send skip-auth configuration");
    let cloned_carrier = resolved
        .transport_headers
        .get("x-pi-bedrock-signing-config")
        .expect("Send private carrier")
        .clone();
    assert!(cloned_carrier.is_sensitive());
    let models = Models::builder()
        .auth_context(Arc::new(skip_auth_context()))
        .provider(send_registration)
        .header_transform(Arc::new(ClonePrivateHeaderValueTransform(cloned_carrier)))
        .build()
        .expect("Send Models with cloned-value transform");
    let _stream = block_on(models.stream_simple(
        ModelRequest {
            model: model("us.anthropic.claude-opus-4-8").common.model_ref,
            context: one_user_context(),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .expect("Send cloned-value transform collision");
    let requests = send_exact_bytes.requests.lock().expect("capture lock");
    let cloned_header = requests[0]
        .headers
        .get("x-pi-bedrock-signing-config")
        .expect("cloned caller replacement is forwarded");
    assert!(cloned_header.is_sensitive());
    drop(requests);

    let local_explicit = Rc::new(LocalCapture::default());
    let models = LocalModels::builder()
        .auth_context(Rc::new(skip_auth_context()))
        .provider(
            local_bedrock_provider(local_explicit.clone()).expect("Local Bedrock registration"),
        )
        .build()
        .expect("Local Models");
    let _stream = block_on(models.stream_simple(
        collision_request("caller-explicit-local"),
        CancellationToken::new(),
    ))
    .expect("Local explicit collision");
    assert_eq!(
        local_explicit.requests.borrow()[0]
            .headers
            .get("x-pi-bedrock-signing-config"),
        Some(&HeaderValue::from_static("caller-explicit-local"))
    );

    let local_transform = Rc::new(LocalCapture::default());
    let models = LocalModels::builder()
        .auth_context(Rc::new(skip_auth_context()))
        .provider(
            local_bedrock_provider(local_transform.clone()).expect("Local Bedrock registration"),
        )
        .header_transform(Rc::new(PrivateHeaderTransform("caller-transform-local")))
        .build()
        .expect("Local Models with transform");
    let _stream = block_on(models.stream_simple(
        collision_request("caller-explicit-before-transform"),
        CancellationToken::new(),
    ))
    .expect("Local transform collision");
    assert_eq!(
        local_transform.requests.borrow()[0]
            .headers
            .get("x-pi-bedrock-signing-config"),
        Some(&HeaderValue::from_static("caller-transform-local"))
    );

    let local_exact_bytes = Rc::new(LocalCapture::default());
    let local_registration =
        local_bedrock_provider(local_exact_bytes.clone()).expect("Local Bedrock registration");
    let mut resolve = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    resolve.auth_context = Rc::new(skip_auth_context());
    let resolved = block_on(
        local_registration
            .auth
            .resolve(resolve, CancellationToken::new()),
    )
    .expect("resolve Local private carrier")
    .expect("Local skip-auth configuration");
    let cloned_carrier = resolved
        .transport_headers
        .get("x-pi-bedrock-signing-config")
        .expect("Local private carrier")
        .clone();
    assert!(cloned_carrier.is_sensitive());
    let models = LocalModels::builder()
        .auth_context(Rc::new(skip_auth_context()))
        .provider(local_registration)
        .header_transform(Rc::new(ClonePrivateHeaderValueTransform(cloned_carrier)))
        .build()
        .expect("Local Models with cloned-value transform");
    let _stream = block_on(models.stream_simple(
        ModelRequest {
            model: model("us.anthropic.claude-opus-4-8").common.model_ref,
            context: one_user_context(),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .expect("Local cloned-value transform collision");
    let requests = local_exact_bytes.requests.borrow();
    let cloned_header = requests[0]
        .headers
        .get("x-pi-bedrock-signing-config")
        .expect("local cloned caller replacement is forwarded");
    assert!(cloned_header.is_sensitive());
}

/// Architecture v2 part 2 §2.6 and §9.2; pinned Pi basis:
/// `node-http-proxy.test.ts`, `node-http-proxy.ts`, and
/// `bedrock-converse-stream.ts:207-219`. Proxy selection is resolved against
/// the request target with scoped-before-ambient aliases and is delivered to
/// both injected signer families together with the HTTP/1 requirement.
#[test]
fn bedrock_proxy_resolution_is_request_scoped_send_and_local_pi_exact() {
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let target = model("us.anthropic.claude-opus-4-8");
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://scoped-proxy.example:8080".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    request.auth_context = Arc::new(MapAuthContext::new(
        [(
            "https_proxy".to_owned(),
            "http://ambient-proxy.example:8080".to_owned(),
        )]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send proxy resolution")
        .expect("Send skip-auth configuration");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(
        config.proxy_url.as_ref().map(Url::as_str),
        Some("http://scoped-proxy.example:8080/")
    );
    assert!(config.force_http1);

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://proxy.example:8080".to_owned(),
            ),
            (
                "NO_PROXY".to_owned(),
                "bedrock-runtime.us-east-1.amazonaws.com".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send NO_PROXY resolution")
        .expect("Send skip-auth configuration");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.proxy_url.is_none());
    assert!(!config.force_http1);

    // JavaScript `/[,\s]/` splits on non-ASCII ECMAScript whitespace too.
    // A no-break space between entries must expose the matching target entry.
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://proxy.example:8080".to_owned(),
            ),
            (
                "NO_PROXY".to_owned(),
                "unrelated.example\u{00a0}bedrock-runtime.us-east-1.amazonaws.com".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send ECMAScript-whitespace NO_PROXY resolution")
        .expect("Send skip-auth configuration");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.proxy_url.is_none());
    assert!(!config.force_http1);

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://proxy.example:8080".to_owned(),
            ),
            (
                "NO_PROXY".to_owned(),
                "bedrock-runtime.us-east-1.amazonaws.com:99999".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send oversized NO_PROXY port resolution")
        .expect("Send skip-auth configuration");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(
        config.proxy_url.as_ref().map(Url::as_str),
        Some("http://proxy.example:8080/")
    );

    let mut http_target = target.clone();
    http_target.common.base_url = Url::parse("http://bedrock.internal").expect("HTTP target");
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(http_target));
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            ("HTTP_PROXY".to_owned(), "proxy.example:8081".to_owned()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("HTTP proxy resolution")
        .expect("HTTP skip-auth configuration");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(
        config.proxy_url.as_ref().map(Url::as_str),
        Some("http://proxy.example:8081/")
    );

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "socks5://proxy.example:1080".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let error = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect_err("SOCKS proxy must be rejected");
    assert_eq!(error.code(), "unsupported_proxy_protocol");
    assert!(error.to_string().contains("Unsupported proxy protocol"));

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(target.clone()),
    );
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://local-proxy.example:8080".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local proxy resolution")
    .expect("Local skip-auth configuration");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(
        config.proxy_url.as_ref().map(Url::as_str),
        Some("http://local-proxy.example:8080/")
    );
    assert!(config.force_http1);

    let mut request =
        LocalResolveAuthRequest::isolated(local_registration.descriptor.clone(), Some(target));
    request.auth_context = Rc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "proxy-test-token".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://local-proxy.example:8080".to_owned(),
            ),
            (
                "NO_PROXY".to_owned(),
                "bedrock-runtime.us-east-1.amazonaws.com:99999".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local oversized NO_PROXY port resolution")
    .expect("Local skip-auth configuration");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(
        config.proxy_url.as_ref().map(Url::as_str),
        Some("http://local-proxy.example:8080/")
    );
}

fn whitespace_overrides() -> AuthResolutionOverrides {
    AuthResolutionOverrides {
        environment: [
            ("AWS_PROFILE".to_owned(), "   ".to_owned()),
            ("AWS_REGION".to_owned(), " \t ".to_owned()),
            ("AWS_BEDROCK_FORCE_HTTP1".to_owned(), " ".to_owned()),
            ("AWS_BEDROCK_FORCE_CACHE".to_owned(), "\t".to_owned()),
            ("PI_CACHE_RETENTION".to_owned(), " ".to_owned()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

fn whitespace_ambient() -> BTreeMap<String, String> {
    [
        ("AWS_PROFILE".to_owned(), "ambient-profile".to_owned()),
        ("AWS_REGION".to_owned(), "us-west-2".to_owned()),
        ("AWS_BEDROCK_FORCE_HTTP1".to_owned(), "1".to_owned()),
        ("AWS_BEDROCK_FORCE_CACHE".to_owned(), "1".to_owned()),
        ("PI_CACHE_RETENTION".to_owned(), "long".to_owned()),
    ]
    .into_iter()
    .collect()
}

fn assert_whitespace_configuration(config: &BedrockSigningConfig) {
    assert_eq!(config.profile.as_deref(), Some("   "));
    assert_eq!(config.region.as_deref(), Some(" \t "));
    assert!(!config.force_http1);
    assert!(!config.force_prompt_caching);
    assert!(!config.long_cache_retention);
}

/// Architecture v2 part 2 §2.6 and §9.2; pinned Pi basis:
/// `provider-env.ts:45-50`. Non-empty whitespace is truthy in pinned Pi and
/// therefore wins over ambient profile, region, credential, and feature values
/// in both resolver families.
#[test]
fn bedrock_provider_environment_whitespace_is_present_send_and_local_pi_exact() {
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let target = model("us.anthropic.claude-opus-4-8");
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.overrides = whitespace_overrides();
    request.auth_context = Arc::new(MapAuthContext::new(whitespace_ambient(), []));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send whitespace resolution")
        .expect("whitespace profile is present");
    let capture = Arc::new(SendCapture::default());
    assert_whitespace_configuration(&send_resolved_auth_to_signer(&capture, &resolved));

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_ACCESS_KEY_ID".to_owned(), " ".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "\t".to_owned()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("Send whitespace credential resolution")
        .expect("whitespace credentials are present");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    let credentials = config.credentials.expect("whitespace credentials");
    assert_eq!(credentials.access_key_id.expose_secret(), " ");
    assert_eq!(credentials.secret_access_key.expose_secret(), "\t");

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(target.clone()),
    );
    request.overrides = whitespace_overrides();
    request.auth_context = Rc::new(MapAuthContext::new(whitespace_ambient(), []));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local whitespace resolution")
    .expect("local whitespace profile is present");
    let capture = Rc::new(LocalCapture::default());
    assert_whitespace_configuration(&local_resolved_auth_to_signer(&capture, &resolved));

    let mut request =
        LocalResolveAuthRequest::isolated(local_registration.descriptor.clone(), Some(target));
    request.overrides = AuthResolutionOverrides {
        environment: [
            ("AWS_ACCESS_KEY_ID".to_owned(), " ".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "\t".to_owned()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    request.auth_context = Rc::new(MapAuthContext::new(
        [
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local whitespace credential resolution")
    .expect("local whitespace credentials are present");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    let credentials = config.credentials.expect("local whitespace credentials");
    assert_eq!(credentials.access_key_id.expose_secret(), " ");
    assert_eq!(credentials.secret_access_key.expose_secret(), "\t");
}

/// Architecture v2 part 1 §3.8, part 2 §6.1 and §10.7; pinned Pi basis:
/// `amazon-bedrock.ts` and `bedrock-credentials.test.ts`.
#[test]
fn bedrock_auth_resolves_bearer_stored_profile_and_ambient_chain() {
    let capture = Arc::new(SendCapture::default());
    let registration = bedrock_provider(capture.clone()).expect("Bedrock provider registration");

    let bearer_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &bearer_store,
        ApiKeyCredential {
            key: Some(SecretString::new("bedrock-token")),
            environment: BTreeMap::new(),
        },
    );
    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = bearer_store;
    let bearer = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("bearer auth resolution")
        .expect("stored bearer credential");
    assert_eq!(
        bearer.headers.get(header::AUTHORIZATION),
        Some(&HeaderValue::from_static("Bearer bedrock-token"))
    );
    assert_eq!(bearer.source.0, "stored credential");
    let config = send_resolved_auth_to_signer(&capture, &bearer);
    assert_eq!(
        config
            .bearer_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("bedrock-token")
    );
    assert!(config.credentials.is_none());

    let profile_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &profile_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), "stored-profile".to_owned())]
                .into_iter()
                .collect(),
        },
    );
    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = profile_store;
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let profile = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("profile auth resolution")
        .expect("stored profile credential");
    assert_eq!(profile.source.0, "stored credential");
    let profile_capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&profile_capture, &profile);
    assert_eq!(config.profile.as_deref(), Some("stored-profile"));
    assert!(
        config.credentials.is_none(),
        "a scoped profile must win over ambient access keys"
    );

    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let ambient = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("ambient auth resolution")
        .expect("ambient AWS access keys");
    assert_eq!(ambient.source.0, "AWS access keys");
    let ambient_capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&ambient_capture, &ambient);
    let credentials = config.credentials.expect("ambient static credentials");
    assert_eq!(credentials.access_key_id.expose_secret(), "ambient-access");
    assert_eq!(
        credentials.secret_access_key.expose_secret(),
        "ambient-secret"
    );

    let local_capture = Rc::new(LocalCapture::default());
    let local =
        local_bedrock_provider(local_capture.clone()).expect("local Bedrock provider registration");
    let mut request = LocalResolveAuthRequest::isolated(local.descriptor.clone(), None);
    request.auth_context = Rc::new(MapAuthContext::new(
        [(
            "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
            "local-token".to_owned(),
        )]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(local.auth.resolve(request, CancellationToken::new()))
        .expect("local bearer auth resolution")
        .expect("local ambient bearer token");
    assert_eq!(
        resolved.headers.get(header::AUTHORIZATION),
        Some(&HeaderValue::from_static("Bearer local-token"))
    );
    let mut signer_request = logical_request(resolved.headers.clone());
    signer_request.auth_headers = resolved_transport_headers(&resolved);
    block_on(
        LocalBedrockSignerTransport::new(local_capture.clone())
            .execute(signer_request, CancellationToken::new()),
    )
    .expect("local signer request");
    assert_eq!(
        local_capture.configs.borrow()[0]
            .bearer_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("local-token")
    );
}

/// Architecture v2 part 2 §6.1, §9.2, and §10.7; pinned Pi basis:
/// `providers/amazon-bedrock.ts:65-72`. The stored environment lookup uses
/// nullish coalescing: a present empty AWS_PROFILE blocks ambient lookup, then
/// fails the profile truthiness check instead of selecting the ambient profile.
#[test]
fn bedrock_stored_empty_profile_blocks_ambient_profile_send_and_local() {
    let send_registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let send_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &send_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), String::new())]
                .into_iter()
                .collect(),
        },
    );
    let mut request = ResolveAuthRequest::isolated(
        send_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = send_store;
    request.auth_context = Arc::new(MapAuthContext::new(
        [("AWS_PROFILE".to_owned(), "ambient-profile".to_owned())]
            .into_iter()
            .collect(),
        [],
    ));
    assert!(
        block_on(
            send_registration
                .auth
                .resolve(request, CancellationToken::new())
        )
        .expect("Send stored-empty profile resolution")
        .is_none()
    );

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local_bedrock_credential(
        &local_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), String::new())]
                .into_iter()
                .collect(),
        },
    );
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = local_store;
    request.auth_context = Rc::new(MapAuthContext::new(
        [("AWS_PROFILE".to_owned(), "ambient-profile".to_owned())]
            .into_iter()
            .collect(),
        [],
    ));
    assert!(
        block_on(
            local_registration
                .auth
                .resolve(request, CancellationToken::new())
        )
        .expect("Local stored-empty profile resolution")
        .is_none()
    );
}

fn ambient_profile_and_access_keys() -> BTreeMap<String, String> {
    [
        ("AWS_PROFILE".to_owned(), "ambient-profile".to_owned()),
        ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
        (
            "AWS_SECRET_ACCESS_KEY".to_owned(),
            "ambient-secret".to_owned(),
        ),
    ]
    .into_iter()
    .collect()
}

fn assert_profile_uses_default_endpoint_chain(
    resolved: &ResolvedAuth,
    config: &BedrockSigningConfig,
    expected_profile: &str,
) {
    assert!(
        resolved.base_url.is_none(),
        "ambient profile presence must leave a standard catalog endpoint unpinned"
    );
    assert_eq!(config.profile.as_deref(), Some(expected_profile));
    assert!(config.endpoint.is_none());
    assert!(config.region.is_none());
}

/// Architecture v2 part 2 §2.6 and §9.2; pinned Pi basis:
/// `providers/amazon-bedrock.ts:65-72` and
/// `api/bedrock-converse-stream.ts:149-198`. Provider-auth eligibility uses
/// the stored-profile nullish check, while Bedrock client construction always
/// observes ambient AWS_PROFILE independently. This distinction applies to a
/// stored/scoped winning profile and to an empty stored profile whose request
/// becomes eligible through ambient access keys.
#[test]
fn bedrock_auth_eligibility_is_independent_from_client_profile_send_and_local() {
    let target = model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0");

    let send_registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let send_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &send_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), "stored-profile".to_owned())]
                .into_iter()
                .collect(),
        },
    );
    let mut request =
        ResolveAuthRequest::isolated(send_registration.descriptor.clone(), Some(target.clone()));
    request.credential_store = send_store;
    request.auth_context = Arc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        send_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Send stored-plus-ambient profile resolution")
    .expect("stored profile auth eligibility");
    assert_eq!(resolved.source.0, "stored credential");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "stored-profile");
    assert!(config.credentials.is_none());

    let mut request =
        ResolveAuthRequest::isolated(send_registration.descriptor.clone(), Some(target.clone()));
    request.overrides = AuthResolutionOverrides {
        environment: [("AWS_PROFILE".to_owned(), "scoped-profile".to_owned())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    request.auth_context = Arc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        send_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Send scoped-plus-ambient profile resolution")
    .expect("scoped profile auth eligibility");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "scoped-profile");
    assert!(config.credentials.is_none());

    let send_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &send_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), String::new())]
                .into_iter()
                .collect(),
        },
    );
    let mut request =
        ResolveAuthRequest::isolated(send_registration.descriptor.clone(), Some(target.clone()));
    request.credential_store = send_store;
    request.auth_context = Arc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        send_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Send empty-stored profile resolution")
    .expect("ambient access-key auth eligibility");
    assert_eq!(resolved.source.0, "AWS access keys");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "ambient-profile");
    assert!(config.credentials.is_some());

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local_bedrock_credential(
        &local_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), "stored-profile".to_owned())]
                .into_iter()
                .collect(),
        },
    );
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(target.clone()),
    );
    request.credential_store = local_store;
    request.auth_context = Rc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local stored-plus-ambient profile resolution")
    .expect("local stored profile auth eligibility");
    assert_eq!(resolved.source.0, "stored credential");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "stored-profile");
    assert!(config.credentials.is_none());

    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(target.clone()),
    );
    request.overrides = AuthResolutionOverrides {
        environment: [("AWS_PROFILE".to_owned(), "scoped-profile".to_owned())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    request.auth_context = Rc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local scoped-plus-ambient profile resolution")
    .expect("local scoped profile auth eligibility");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "scoped-profile");
    assert!(config.credentials.is_none());

    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local_bedrock_credential(
        &local_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), String::new())]
                .into_iter()
                .collect(),
        },
    );
    let mut request =
        LocalResolveAuthRequest::isolated(local_registration.descriptor.clone(), Some(target));
    request.credential_store = local_store;
    request.auth_context = Rc::new(MapAuthContext::new(ambient_profile_and_access_keys(), []));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local empty-stored profile resolution")
    .expect("local ambient access-key auth eligibility");
    assert_eq!(resolved.source.0, "AWS access keys");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert_profile_uses_default_endpoint_chain(&resolved, &config, "ambient-profile");
    assert!(config.credentials.is_some());
}

/// Architecture v2 part 2 §2.6; pinned Pi basis:
/// `bedrock-credentials.test.ts` ambient-profile/static-key cases and
/// `api/bedrock-converse-stream.ts:145-198`.
#[test]
fn bedrock_ambient_profile_and_static_keys_reach_signer_pi_exact() {
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Bedrock provider registration");
    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0")),
    );
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_PROFILE".to_owned(), "ambient-profile".to_owned()),
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
            ("AWS_SESSION_TOKEN".to_owned(), "ambient-session".to_owned()),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("ambient profile resolution")
        .expect("ambient profile auth");
    assert!(resolved.base_url.is_none());
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.profile.as_deref(), Some("ambient-profile"));
    assert!(config.region.is_none());
    assert!(config.endpoint.is_none());
    let credentials = config.credentials.expect("ambient static credentials");
    assert_eq!(credentials.access_key_id.expose_secret(), "ambient-access");
    assert_eq!(
        credentials.secret_access_key.expose_secret(),
        "ambient-secret"
    );
    assert_eq!(
        credentials
            .session_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("ambient-session")
    );
}

/// Architecture v2 part 2 §2.6 and §9.2; pinned Pi basis:
/// `providers/amazon-bedrock.ts:61-78` decides provider eligibility without
/// consulting AWS_BEDROCK_SKIP_AUTH, while
/// `api/bedrock-converse-stream.ts:170-204` applies skip-auth only when
/// configuring the direct Bedrock client. A bearer source therefore remains
/// the source under skip-auth, and skip-auth alone is not provider auth.
#[test]
fn bedrock_skip_auth_applies_after_provider_eligibility_send_and_local_pi_exact() {
    let send_registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");

    let mut skip_only = ResolveAuthRequest::isolated(
        send_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    skip_only.auth_context = Arc::new(MapAuthContext::new(
        [("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned())]
            .into_iter()
            .collect(),
        [],
    ));
    assert!(
        block_on(
            send_registration
                .auth
                .resolve(skip_only, CancellationToken::new())
        )
        .expect("Send skip-only resolution")
        .is_none()
    );

    let mut request = ResolveAuthRequest::isolated(
        send_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "must-not-be-used".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(
        send_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Send bearer plus skip-auth resolution")
    .expect("Send bearer remains an eligible auth source");
    assert_eq!(resolved.source.0, "AWS_BEARER_TOKEN_BEDROCK");
    assert!(resolved.headers.get(header::AUTHORIZATION).is_none());
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.skip_auth);
    assert!(config.bearer_token.is_none());
    let credentials = config.credentials.expect("dummy skip-auth credentials");
    assert_eq!(
        credentials.access_key_id.expose_secret(),
        "dummy-access-key"
    );
    assert_eq!(
        credentials.secret_access_key.expose_secret(),
        "dummy-secret-key"
    );

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let mut skip_only = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    skip_only.auth_context = Rc::new(MapAuthContext::new(
        [("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned())]
            .into_iter()
            .collect(),
        [],
    ));
    assert!(
        block_on(
            local_registration
                .auth
                .resolve(skip_only, CancellationToken::new())
        )
        .expect("Local skip-only resolution")
        .is_none()
    );

    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.auth_context = Rc::new(MapAuthContext::new(
        [
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "must-not-be-used".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local bearer plus skip-auth resolution")
    .expect("Local bearer remains an eligible auth source");
    assert_eq!(resolved.source.0, "AWS_BEARER_TOKEN_BEDROCK");
    assert!(resolved.headers.get(header::AUTHORIZATION).is_none());
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.skip_auth);
    assert!(config.bearer_token.is_none());
    let credentials = config.credentials.expect("local dummy credentials");
    assert_eq!(
        credentials.access_key_id.expose_secret(),
        "dummy-access-key"
    );
    assert_eq!(
        credentials.secret_access_key.expose_secret(),
        "dummy-secret-key"
    );
}

/// Architecture v2 part 2 §2.6 and §9.2; pinned Pi basis:
/// `providers/amazon-bedrock.ts:61-70` returns an ambient bearer result without
/// the stored credential environment, so `Models.applyAuth` cannot pass a
/// losing stored profile to `api/bedrock-converse-stream.ts:145-180`.
#[test]
fn bedrock_ambient_bearer_drops_stored_profile_environment_send_and_local_pi_exact() {
    let target = model("us.anthropic.claude-opus-4-8");
    let stored_environment: BTreeMap<String, String> = [
        ("AWS_PROFILE".to_owned(), "stored-profile".to_owned()),
        ("AWS_REGION".to_owned(), "eu-west-3".to_owned()),
    ]
    .into_iter()
    .collect();
    let ambient_bearer: BTreeMap<String, String> = [(
        "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
        "ambient-token".to_owned(),
    )]
    .into_iter()
    .collect();

    let send_registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let send_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &send_store,
        ApiKeyCredential {
            key: None,
            environment: stored_environment.clone(),
        },
    );
    let mut request =
        ResolveAuthRequest::isolated(send_registration.descriptor.clone(), Some(target.clone()));
    request.credential_store = send_store;
    request.auth_context = Arc::new(MapAuthContext::new(ambient_bearer.clone(), []));
    let resolved = block_on(
        send_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Send ambient bearer resolution")
    .expect("Send ambient bearer auth");
    assert_eq!(resolved.source.0, "AWS_BEARER_TOKEN_BEDROCK");
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.profile.is_none());
    assert_eq!(config.region.as_deref(), Some("us-east-1"));
    assert_eq!(config.endpoint.as_ref(), Some(&target.common.base_url));
    assert_eq!(
        config
            .bearer_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("ambient-token")
    );

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local_bedrock_credential(
        &local_store,
        ApiKeyCredential {
            key: None,
            environment: stored_environment,
        },
    );
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(target.clone()),
    );
    request.credential_store = local_store;
    request.auth_context = Rc::new(MapAuthContext::new(ambient_bearer, []));
    let resolved = block_on(
        local_registration
            .auth
            .resolve(request, CancellationToken::new()),
    )
    .expect("Local ambient bearer resolution")
    .expect("Local ambient bearer auth");
    assert_eq!(resolved.source.0, "AWS_BEARER_TOKEN_BEDROCK");
    let capture = Rc::new(LocalCapture::default());
    let config = local_resolved_auth_to_signer(&capture, &resolved);
    assert!(config.profile.is_none());
    assert_eq!(config.region.as_deref(), Some("us-east-1"));
    assert_eq!(config.endpoint.as_ref(), Some(&target.common.base_url));
    assert_eq!(
        config
            .bearer_token
            .as_ref()
            .map(SecretString::expose_secret),
        Some("ambient-token")
    );
}

/// Architecture v2 part 2 §6.1, §9.2, and §10.7; pinned Pi basis:
/// `providers/amazon-bedrock.ts:61-78` consults stored credential environment
/// only for AWS_PROFILE. Stored bearer/static/role fields alone cannot make the
/// provider configured.
#[test]
fn bedrock_stored_non_profile_environment_does_not_configure_provider_send_and_local_pi_exact() {
    let stored_environment: BTreeMap<String, String> = [
        (
            "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
            "stored-token".to_owned(),
        ),
        ("AWS_ACCESS_KEY_ID".to_owned(), "stored-access".to_owned()),
        (
            "AWS_SECRET_ACCESS_KEY".to_owned(),
            "stored-secret".to_owned(),
        ),
        (
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI".to_owned(),
            "/stored-role".to_owned(),
        ),
    ]
    .into_iter()
    .collect();

    let send_registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let send_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &send_store,
        ApiKeyCredential {
            key: None,
            environment: stored_environment.clone(),
        },
    );
    let mut request = ResolveAuthRequest::isolated(
        send_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = send_store;
    assert!(
        block_on(
            send_registration
                .auth
                .resolve(request, CancellationToken::new())
        )
        .expect("Send stored non-profile environment resolution")
        .is_none()
    );

    let local_registration = local_bedrock_provider(Rc::new(LocalCapture::default()))
        .expect("Local Bedrock registration");
    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local_bedrock_credential(
        &local_store,
        ApiKeyCredential {
            key: None,
            environment: stored_environment,
        },
    );
    let mut request = LocalResolveAuthRequest::isolated(
        local_registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.credential_store = local_store;
    assert!(
        block_on(
            local_registration
                .auth
                .resolve(request, CancellationToken::new())
        )
        .expect("Local stored non-profile environment resolution")
        .is_none()
    );
}

/// Architecture v2 part 2 §2.6 and §5.1; pinned Pi basis:
/// `bedrock-endpoint-resolution.test.ts`.
#[test]
fn bedrock_region_and_arn_route_the_effective_endpoint() {
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Bedrock provider registration");
    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(model("us.anthropic.claude-opus-4-8")),
    );
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_REGION".to_owned(), "us-east-2".to_owned()),
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("regional auth resolution")
        .expect("ambient auth");
    assert!(
        resolved.base_url.is_none(),
        "AWS_REGION leaves a standard catalog endpoint unpinned"
    );
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.region.as_deref(), Some("us-east-2"));
    assert!(config.endpoint.is_none());

    let mut arn_model = model("us.anthropic.claude-opus-4-8");
    arn_model.common.model_ref.model = ModelId::new(
        "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123",
    );
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(arn_model));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_REGION".to_owned(), "us-east-1".to_owned()),
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("ARN auth resolution")
        .expect("ambient auth");
    assert!(resolved.base_url.is_none());
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.region.as_deref(), Some("us-gov-west-1"));
    assert!(config.endpoint.is_none());
}

/// Architecture v2 part 2 §2.6; pinned Pi basis:
/// `bedrock-endpoint-resolution.test.ts` default, scoped-profile, and custom-endpoint cases.
#[test]
fn bedrock_endpoint_resolution_preserves_pi_profile_and_custom_endpoint_rules() {
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Bedrock provider registration");

    let eu_model = model("eu.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(eu_model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_ACCESS_KEY_ID".to_owned(), "ambient-access".to_owned()),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "ambient-secret".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("default endpoint resolution")
        .expect("ambient access keys");
    assert_eq!(resolved.base_url.as_ref(), Some(&eu_model.common.base_url));
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));
    assert_eq!(config.endpoint.as_ref(), Some(&eu_model.common.base_url));

    let profile_store = Arc::new(InMemoryCredentialStore::new());
    put_bedrock_credential(
        &profile_store,
        ApiKeyCredential {
            key: None,
            environment: [("AWS_PROFILE".to_owned(), "scoped-profile".to_owned())]
                .into_iter()
                .collect(),
        },
    );
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(eu_model.clone()));
    request.credential_store = profile_store;
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("scoped profile endpoint resolution")
        .expect("scoped profile");
    assert_eq!(resolved.base_url.as_ref(), Some(&eu_model.common.base_url));
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.profile.as_deref(), Some("scoped-profile"));
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));

    let mut custom = eu_model;
    custom.common.base_url = Url::parse("http://127.0.0.1:4567/custom").expect("custom URL");
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(custom.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(
        [
            ("AWS_REGION".to_owned(), "ap-southeast-2".to_owned()),
            ("AWS_BEDROCK_SKIP_AUTH".to_owned(), "1".to_owned()),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "endpoint-test-token".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(registration.auth.resolve(request, CancellationToken::new()))
        .expect("custom endpoint resolution")
        .expect("skip-auth custom endpoint");
    assert_eq!(resolved.base_url.as_ref(), Some(&custom.common.base_url));
    let capture = Arc::new(SendCapture::default());
    let config = send_resolved_auth_to_signer(&capture, &resolved);
    assert_eq!(config.region.as_deref(), Some("ap-southeast-2"));
    assert_eq!(config.endpoint.as_ref(), Some(&custom.common.base_url));
    assert!(config.skip_auth);
}

/// Architecture v2 part 2 §1.7 and §10.8; pinned Pi basis:
/// `bedrock-thinking-payload.test.ts` GovCloud cases.
#[test]
fn bedrock_govcloud_omits_thinking_display_pi_exact() {
    let adaptive = model("global.anthropic.claude-opus-5");
    let body = encode_command(
        &adaptive,
        &one_user_context(),
        &BedrockOptions {
            region: Some("us-gov-west-1".to_owned()),
            reasoning: Some(ReasoningLevel::High),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""thinking":{"type":"adaptive"}"#));
    assert!(!body.contains("display"));

    let mut budget = model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    budget.common.model_ref.model =
        ModelId::new("us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let body = encode_command(
        &budget,
        &one_user_context(),
        &BedrockOptions {
            reasoning: Some(ReasoningLevel::High),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""thinking":{"type":"enabled","budget_tokens":16384}"#));
    assert!(!body.contains("display"));
}

/// Architecture v2 part 2 §5.1 and §10.8; pinned Pi basis:
/// `bedrock-thinking-payload.test.ts` and
/// `bedrock-converse-stream.ts:768-775,1206-1260`.
#[test]
fn bedrock_thinking_levels_pi_exact() {
    for model_id in [
        "global.anthropic.claude-fable-5",
        "global.anthropic.claude-sonnet-5",
        "global.anthropic.claude-opus-5",
    ] {
        let body = encode_command(
            &model(model_id),
            &one_user_context(),
            &BedrockOptions {
                reasoning: Some(ReasoningLevel::High),
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        );
        assert!(body.contains(
            r#""thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}"#
        ));
        assert!(!body.contains("anthropic_beta"));
    }

    let adaptive = model("global.anthropic.claude-opus-5");
    for (level, effort) in [
        (ReasoningLevel::Minimal, "low"),
        (ReasoningLevel::Low, "low"),
        (ReasoningLevel::Medium, "medium"),
        (ReasoningLevel::High, "high"),
        (ReasoningLevel::Xhigh, "xhigh"),
        (ReasoningLevel::Max, "max"),
    ] {
        let body = encode_command(
            &adaptive,
            &one_user_context(),
            &BedrockOptions {
                reasoning: Some(level),
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        );
        assert!(body.contains(&format!(
            r#""additionalModelRequestFields":{{"thinking":{{"type":"adaptive","display":"summarized"}},"output_config":{{"effort":"{effort}"}}}}"#
        )));
    }

    // Pinned mapThinkingLevelToEffort recognizes native xhigh before reading
    // thinkingLevelMap. A catalog override cannot downgrade Opus 4.8 xhigh.
    let mut native_xhigh = model("us.anthropic.claude-opus-4-8");
    let agentprism_ai::ApiModelConfig::BedrockConverse(config) = &mut native_xhigh.api else {
        panic!("Bedrock config")
    };
    config.thinking_levels.xhigh = Some(agentprism_ai::LevelSupport::Value("high".to_owned()));
    let body = encode_command(
        &native_xhigh,
        &one_user_context(),
        &BedrockOptions {
            reasoning: Some(ReasoningLevel::Xhigh),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""output_config":{"effort":"xhigh"}"#));
    assert!(!body.contains(r#""output_config":{"effort":"high"}"#));

    let budget = model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    for (level, tokens) in [
        (ReasoningLevel::Minimal, 1_024),
        (ReasoningLevel::Low, 2_048),
        (ReasoningLevel::Medium, 8_192),
        (ReasoningLevel::High, 16_384),
        (ReasoningLevel::Xhigh, 16_384),
        (ReasoningLevel::Max, 16_384),
    ] {
        let body = encode_command(
            &budget,
            &one_user_context(),
            &BedrockOptions {
                reasoning: Some(level),
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        );
        assert!(body.contains(&format!(r#""budget_tokens":{tokens}"#)));
    }
}

/// Architecture v2 part 2 §1.7 and §10.8; pinned Pi basis:
/// `bedrock-thinking-payload.test.ts` application inference profile cases and
/// `bedrock-converse-stream.ts::getModelMatchCandidates`.
#[test]
fn bedrock_application_inference_profile_name_controls_thinking_and_cache() {
    let mut adaptive = model("global.anthropic.claude-opus-4-6-v1");
    adaptive.common.model_ref.model = ModelId::new(
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/thinking-profile",
    );
    adaptive.common.display_name = "Claude Opus 4.6".to_owned();
    let body = encode_command(
        &adaptive,
        &context_with_system_prompt(),
        &BedrockOptions {
            reasoning: Some(ReasoningLevel::High),
            ..Default::default()
        },
    );
    assert!(body.contains(
        r#""thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}"#
    ));
    assert_eq!(body.matches(r#""cachePoint""#).count(), 2);

    // Pi normalizes `/[\s_.:]+/g` to one hyphen. Repeated separators and
    // non-space ECMAScript whitespace must therefore still identify Opus 4.6.
    for display_name in ["Claude__Opus__4__6", "Claude\u{00a0}Opus\u{feff}4\u{2028}6"] {
        let mut normalized = adaptive.clone();
        normalized.common.display_name = display_name.to_owned();
        let body = encode_command(
            &normalized,
            &one_user_context(),
            &BedrockOptions {
                reasoning: Some(ReasoningLevel::High),
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            },
        );
        assert!(
            body.contains(
                r#""thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}"#
            ),
            "display name {display_name:?} must enable Pi-equivalent adaptive thinking"
        );
    }

    let mut fixed = model("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    fixed.common.model_ref.model = ModelId::new(
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/fixed-profile",
    );
    fixed.common.display_name = "Claude Sonnet 4.5".to_owned();
    let body = encode_command(
        &fixed,
        &one_user_context(),
        &BedrockOptions {
            reasoning: Some(ReasoningLevel::High),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        },
    );
    assert!(body.contains(r#""thinking":{"type":"enabled","budget_tokens":16384"#));
    assert!(body.contains(r#""anthropic_beta":["interleaved-thinking-2025-05-14"]"#));
}

/// Architecture v2 part 2 §2.1 and §10.1; pinned Pi basis:
/// `bedrock-error-metadata.test.ts` and Bedrock's terminal cost calculation.
#[test]
fn bedrock_failure_and_cancellation_preserve_usage_cost_and_request_id() {
    let model = model("us.anthropic.claude-opus-4-8");
    for cancelled in [false, true] {
        let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
            message_id: MessageId::new(if cancelled { "cancelled" } else { "failed" }),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            pricing: model.common.pricing.clone(),
            timestamp: Timestamp::default(),
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-amzn-requestid", HeaderValue::from_static("req-123"));
        decoder.observe_response(200, &headers);
        let _ = decoder.take_events();
        let _ = decoder.push_event(
            "metadata",
            &json!({"usage":{"inputTokens":1000000,"outputTokens":1000000}}),
        );
        let events = if cancelled {
            decoder.cancel("aborted")
        } else {
            decoder.fail_transport("stream", "mid-stream failure")
        };
        let message = terminal(&events);
        assert_eq!(
            message.cost.as_ref().expect("terminal cost").micros,
            30_000_000
        );
        if cancelled {
            assert_eq!(
                message
                    .finish
                    .error
                    .as_ref()
                    .and_then(|error| error.request_id.as_deref()),
                Some("req-123")
            );
        } else {
            assert_eq!(
                message
                    .finish
                    .error
                    .as_ref()
                    .and_then(|error| error.request_id.as_deref()),
                Some("req-123")
            );
            assert_eq!(
                message
                    .diagnostics
                    .iter()
                    .find(|diagnostic| diagnostic.kind == "bedrock_response_failure")
                    .and_then(|diagnostic| diagnostic.details.get("requestId")),
                Some(&json!("req-123"))
            );
        }
    }

    // Pi's diagnostic normalization uses ECMAScript trim: U+FEFF is removed,
    // while U+0085 remains part of the request id.
    for (request_id, expected) in [
        ("\u{feff}request-feff\u{feff}", "request-feff"),
        (
            "\u{0085}request-0085\u{0085}",
            "\u{0085}request-0085\u{0085}",
        ),
    ] {
        let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
            message_id: MessageId::new("diagnostic-trim"),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            pricing: model.common.pricing.clone(),
            timestamp: Timestamp::default(),
        });
        let _ = decoder.take_events();
        let message = terminal(&decoder.fail_transport_error(
            TransportError::new("stream", "mid-stream failure").with_request_id(request_id),
        ));
        assert_eq!(
            bedrock_failure_details(&message).and_then(|details| details.get("requestId").cloned()),
            Some(json!(expected))
        );
    }
}

fn send_start_failure(failure: BedrockProviderFailure) -> agentprism_ai::AiError {
    let model = model("us.anthropic.claude-opus-4-8");
    send_start_failure_with_request(failure, resolved_request(model))
}

fn send_start_failure_with_request(
    failure: BedrockProviderFailure,
    request: ResolvedApiRequest,
) -> agentprism_ai::AiError {
    let api = bedrock_converse_stream_api(Arc::new(TypedFailureSigner {
        point: TypedFailurePoint::Establishment,
        failure,
    }));
    match block_on(api.stream(request, CancellationToken::new())) {
        Ok(_) => panic!("typed signer failure must reject stream establishment"),
        Err(error) => error,
    }
}

fn local_start_failure(failure: BedrockProviderFailure) -> agentprism_ai::AiError {
    let model = model("us.anthropic.claude-opus-4-8");
    local_start_failure_with_request(failure, local_resolved_request(model))
}

fn local_start_failure_with_request(
    failure: BedrockProviderFailure,
    request: LocalResolvedApiRequest,
) -> agentprism_ai::AiError {
    let api = local_bedrock_converse_stream_api(Rc::new(LocalTypedFailureSigner {
        point: TypedFailurePoint::Establishment,
        failure,
    }));
    match block_on(api.stream(request, CancellationToken::new())) {
        Ok(_) => panic!("typed local signer failure must reject stream establishment"),
        Err(error) => error,
    }
}

fn send_body_failure(failure: BedrockProviderFailure) -> agentprism_ai::AssistantMessage {
    let model = model("us.anthropic.claude-opus-4-8");
    send_body_failure_with_request(failure, resolved_request(model))
}

fn send_body_failure_with_request(
    failure: BedrockProviderFailure,
    request: ResolvedApiRequest,
) -> agentprism_ai::AssistantMessage {
    let api = bedrock_converse_stream_api(Arc::new(TypedFailureSigner {
        point: TypedFailurePoint::Body,
        failure,
    }));
    let mut stream = block_on(api.stream(request, CancellationToken::new()))
        .expect("body failure occurs after stream establishment");
    let events = block_on(async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    });
    terminal(&events)
}

fn local_body_failure(failure: BedrockProviderFailure) -> agentprism_ai::AssistantMessage {
    let model = model("us.anthropic.claude-opus-4-8");
    local_body_failure_with_request(failure, local_resolved_request(model))
}

fn local_body_failure_with_request(
    failure: BedrockProviderFailure,
    request: LocalResolvedApiRequest,
) -> agentprism_ai::AssistantMessage {
    let api = local_bedrock_converse_stream_api(Rc::new(LocalTypedFailureSigner {
        point: TypedFailurePoint::Body,
        failure,
    }));
    let mut stream = block_on(api.stream(request, CancellationToken::new()))
        .expect("local body failure occurs after stream establishment");
    let events = block_on(async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    });
    terminal(&events)
}

fn bedrock_failure_details(
    message: &agentprism_ai::AssistantMessage,
) -> Option<BTreeMap<String, serde_json::Value>> {
    message
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.kind == "bedrock_response_failure")
        .map(|diagnostic| diagnostic.details.clone())
}

/// Architecture v2 part 2 §2.1, §2.6, and §9.2; pinned Pi basis:
/// `bedrock-error-metadata.test.ts`, `provider-error-body-regression.test.ts`,
/// and `bedrock-converse-stream.ts:340-397`. Both ChatApi trait families must
/// normalize the typed signer failure rather than accepting preformatted text.
#[test]
fn bedrock_error_metadata_normalization_pi_exact() {
    let request_id = "11111111-2222-3333-4444-555555555555";
    let validation = BedrockProviderFailure::service(
        "ValidationException",
        "The provided model identifier is invalid.",
    )
    .with_status(400)
    .with_request_id(request_id);
    for error in [
        send_start_failure(validation.clone()),
        local_start_failure(validation),
    ] {
        assert_eq!(error.kind, agentprism_ai::AiErrorKind::ProviderRejected);
        assert_eq!(
            error.message,
            "Validation error: The provided model identifier is invalid."
        );
        assert_eq!(error.status, Some(400));
        assert_eq!(error.provider_code.as_deref(), Some("ValidationException"));
        assert_eq!(error.request_id.as_deref(), Some(request_id));
    }

    let raw_gateway = BedrockProviderFailure::new("UnknownError")
        .with_status(403)
        .with_body(r#"{"message":"blocked by gateway WAF"}"#);
    for error in [
        send_start_failure(raw_gateway.clone()),
        local_start_failure(raw_gateway),
    ] {
        assert_eq!(error.kind, agentprism_ai::AiErrorKind::Authorization);
        assert_eq!(
            error.message,
            r#"403: {"message":"blocked by gateway WAF"}"#
        );
        assert_eq!(error.status, Some(403));
        assert!(!error.message.contains("Unknown: UnknownError"));
    }

    for (request_id, expected) in [
        ("\u{feff}request-feff\u{feff}", "request-feff"),
        (
            "\u{0085}request-0085\u{0085}",
            "\u{0085}request-0085\u{0085}",
        ),
    ] {
        let error = send_start_failure(
            BedrockProviderFailure::new("request-id trim").with_request_id(request_id),
        );
        assert_eq!(error.request_id.as_deref(), Some(expected));
    }

    for (body, expected) in [
        ("\u{feff}gateway body\u{feff}", "403: gateway body"),
        (
            "\u{0085}gateway body\u{0085}",
            "403: \u{0085}gateway body\u{0085}",
        ),
    ] {
        let error = send_start_failure(
            BedrockProviderFailure::new("UnknownError")
                .with_status(403)
                .with_body(body),
        );
        assert_eq!(error.message, expected);
    }
}

/// Architecture v2 part 2 §2.1, §2.6, §9.2, and §10.4; pinned Pi basis:
/// `bedrock-error-metadata.test.ts` and
/// `provider-error-body-regression.test.ts`. Established Send and Local body
/// failures retain formatter output and structured metadata in-band.
#[test]
fn bedrock_body_error_normalization_pi_exact_send_and_local() {
    let request_id = "11111111-2222-3333-4444-555555555555";
    let failure = BedrockProviderFailure::service("ValidationException", "UnknownError")
        .with_status(400)
        .with_body(r#"{"message":"data retention mode 'default' is not available for this model"}"#)
        .with_request_id(request_id);
    for message in [
        send_body_failure(failure.clone()),
        local_body_failure(failure),
    ] {
        let error = message
            .finish
            .error
            .as_ref()
            .expect("failed terminal error");
        assert_eq!(error.status, Some(400));
        assert_eq!(error.provider_code.as_deref(), Some("ValidationException"));
        assert_eq!(error.request_id.as_deref(), Some(request_id));
        assert_eq!(
            error.message,
            concat!(
                "Validation error: 400: {\"message\":\"data retention mode 'default' is not available for this model\"}",
                " See https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html for supported data retention modes."
            )
        );
        assert_eq!(
            bedrock_failure_details(&message),
            Some(
                [
                    ("errorCode".to_owned(), json!("ValidationException")),
                    ("requestId".to_owned(), json!(request_id)),
                    ("status".to_owned(), json!(400)),
                ]
                .into_iter()
                .collect()
            )
        );
    }

    let unmodeled = BedrockProviderFailure::new("Model stream terminated unexpectedly.")
        .with_provider_code("ModelStreamErrorException")
        .with_request_id(request_id);
    let message = send_body_failure(unmodeled);
    assert_eq!(
        bedrock_failure_details(&message),
        Some(
            [
                ("errorCode".to_owned(), json!("ModelStreamErrorException")),
                ("requestId".to_owned(), json!(request_id)),
            ]
            .into_iter()
            .collect()
        )
    );

    let long_code = format!("{}Exception", "E".repeat(5_000));
    let oversized = BedrockProviderFailure::new("oversized metadata")
        .with_provider_code(long_code)
        .with_status(400)
        .with_request_id("R".repeat(5_000));
    assert_eq!(
        bedrock_failure_details(&send_body_failure(oversized)),
        Some([("status".to_owned(), json!(400))].into_iter().collect())
    );
}

/// Architecture v2 part 2 §10.1 `stream_error_sanitizes_secrets` and §9.2;
/// native hardening at the Send and Local established-body boundaries. Pinned
/// Pi basis: `provider-error-body-regression.test.ts` and
/// `bedrock-converse-stream.ts:340-397`.
#[test]
fn bedrock_body_stream_failure_sanitizes_secrets_send_and_local() {
    const AUTH_SECRET: &str = "Bearer request-auth-secret";
    const API_KEY_SECRET: &str = "request-api-secret";
    const BODY_SECRET: &str = "body-secret";
    let failure = BedrockProviderFailure::new("UnknownError")
        .with_status(403)
        .with_body(format!(
            r#"{{"access_token":"{BODY_SECRET}","authorization":"{AUTH_SECRET}","api_key":"{API_KEY_SECRET}"}}"#
        ));

    let model = model("us.anthropic.claude-opus-4-8");
    let mut send_request = resolved_request(model.clone());
    let mut authorization = HeaderValue::from_static(AUTH_SECRET);
    authorization.set_sensitive(true);
    let mut api_key = HeaderValue::from_static(API_KEY_SECRET);
    api_key.set_sensitive(true);
    send_request
        .headers
        .insert(header::AUTHORIZATION, authorization.clone());
    send_request.headers.insert("x-api-key", api_key.clone());
    send_request
        .auth_headers
        .insert(header::AUTHORIZATION, authorization.clone());
    send_request
        .auth_headers
        .insert("x-api-key", api_key.clone());

    let mut local_request = local_resolved_request(model);
    local_request
        .headers
        .insert(header::AUTHORIZATION, authorization.clone());
    local_request.headers.insert("x-api-key", api_key.clone());
    local_request
        .auth_headers
        .insert(header::AUTHORIZATION, authorization);
    local_request.auth_headers.insert("x-api-key", api_key);

    for message in [
        send_body_failure_with_request(failure.clone(), send_request),
        local_body_failure_with_request(failure, local_request),
    ] {
        let persisted = serde_json::to_string(&message).expect("persist failed assistant");
        assert!(!persisted.contains(AUTH_SECRET));
        assert!(!persisted.contains(API_KEY_SECRET));
        assert!(!persisted.contains(BODY_SECRET));
        let error = message.finish.error.expect("failed terminal error");
        assert_eq!(error.status, Some(403));
        assert_eq!(
            error.message,
            r#"403: {"access_token":"[REDACTED]","authorization":"[REDACTED]","api_key":"[REDACTED]"}"#
        );
    }
}

fn bedrock_signing_secret_requests() -> (ResolvedApiRequest, LocalResolvedApiRequest) {
    let target = model("us.anthropic.claude-opus-4-8");
    let registration =
        bedrock_provider(Arc::new(SendCapture::default())).expect("Send Bedrock registration");
    let mut resolution =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(target.clone()));
    resolution.auth_context = Arc::new(MapAuthContext::new(
        [
            (
                "AWS_ACCESS_KEY_ID".to_owned(),
                "signing-access-id".to_owned(),
            ),
            (
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "signing-secret-value".to_owned(),
            ),
            (
                "AWS_SESSION_TOKEN".to_owned(),
                "signing-session-value".to_owned(),
            ),
            (
                "AWS_BEARER_TOKEN_BEDROCK".to_owned(),
                "signing-bearer-value".to_owned(),
            ),
            (
                "HTTPS_PROXY".to_owned(),
                "http://signing-proxy-user:signing-proxy-password@proxy.example:8080".to_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        [],
    ));
    let resolved = block_on(
        registration
            .auth
            .resolve(resolution, CancellationToken::new()),
    )
    .expect("Bedrock signing-secret auth resolution")
    .expect("Bedrock signing-secret configuration");

    let mut send = resolved_request(target);
    send.auth_headers = resolved_transport_headers(&resolved);
    let local = LocalResolvedApiRequest {
        model: send.model.clone(),
        context: send.context.clone(),
        options: send.options.clone(),
        full_options: None,
        request_options: send.request_options.clone(),
        endpoint: send.endpoint.clone(),
        headers: send.headers.clone(),
        auth_headers: send.auth_headers.clone(),
        api_key: send.api_key.clone(),
        api: send.api.clone(),
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: send.retry_policy.clone(),
        timeout: send.timeout,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    };
    (send, local)
}

/// Architecture v2 part 2 §10.1 `stream_error_sanitizes_secrets` and §9.2;
/// native hardening for private Bedrock signer configuration at both the
/// pre-stream and established-body boundaries. Pinned Pi basis:
/// `provider-error-body-regression.test.ts` and
/// `bedrock-converse-stream.ts:340-397`.
#[test]
fn stream_error_sanitizes_secrets_bedrock_signing_start_and_body_send_and_local() {
    const SECRETS: [&str; 6] = [
        "signing-access-id",
        "signing-secret-value",
        "signing-session-value",
        "signing-bearer-value",
        "signing-proxy-user",
        "signing-proxy-password",
    ];
    let echoed = format!(
        "signer echoed first={}; second={}; third={}; fourth={}; fifth={}; sixth={}",
        SECRETS[0], SECRETS[1], SECRETS[2], SECRETS[3], SECRETS[4], SECRETS[5]
    );
    let failure = BedrockProviderFailure::new(echoed);
    let (send_start_request, local_start_request) = bedrock_signing_secret_requests();

    for error in [
        send_start_failure_with_request(failure.clone(), send_start_request),
        local_start_failure_with_request(failure.clone(), local_start_request),
    ] {
        for secret in SECRETS {
            assert!(!error.message.contains(secret));
        }
        assert_eq!(error.message.matches("[REDACTED]").count(), SECRETS.len());
    }

    let (send_body_request, local_body_request) = bedrock_signing_secret_requests();
    for message in [
        send_body_failure_with_request(failure.clone(), send_body_request),
        local_body_failure_with_request(failure, local_body_request),
    ] {
        let persisted = serde_json::to_string(&message).expect("persist failed assistant");
        for secret in SECRETS {
            assert!(!persisted.contains(secret));
        }
        let error = message.finish.error.expect("failed terminal error");
        assert_eq!(error.message.matches("[REDACTED]").count(), SECRETS.len());
    }
}

/// Architecture v2 part 2 §2.1; pinned Pi basis:
/// `bedrock-raw-stop-reason.test.ts`.
#[test]
fn bedrock_raw_stop_reason_is_preserved() {
    let model = model("us.anthropic.claude-opus-4-8");
    let mut decoder = BedrockConverseDecoder::new(BedrockDecodeContext {
        message_id: MessageId::new("raw-stop"),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
        timestamp: Timestamp::default(),
    });
    let _ = decoder.take_events();
    let _ = decoder.push_event("messageStop", &json!({"stopReason":"guardrail_intervened"}));
    let message = terminal(&decoder.finish());
    assert_eq!(
        message.finish.raw_provider_reason.as_deref(),
        Some("guardrail_intervened")
    );
}
