//! pi-messages §10.8 wire and stream conformance.

use pi_ai::*;
use pi_ai_pi_messages::*;
use serde_json::value::RawValue;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use url::Url;

#[path = "../../fixtures/rust_support.rs"]
mod fixture;

fn typed_model() -> TypedModelDescriptor<PiMessages> {
    TypedModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("radius", "auto"),
            display_name: "Radius Auto".into(),
            base_url: Url::parse("https://radius.pi.dev/v1").unwrap(),
            modalities: ModalityCapabilities {
                input: BTreeSet::from([Modality::Text]),
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: 128_000,
                max_output_tokens: 16_384,
            },
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: false,
            headers: HeaderMapSpec::new(),
        },
        config: CustomApiModelConfig {
            api: ApiId::new("pi-messages"),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        },
        extensions: ExtensionMap::new(),
    }
}

#[test]
fn wire_pi_messages_basic_pi_exact() {
    // §10.8; Pi basis: api/pi-messages.ts `stream`, where JSON.stringify
    // observes `{ model, context, options }` and legacy message shapes.
    let model = typed_model();
    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("rust-only-id"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("rust-only-block-id"),
            text: "Hello".into(),
        }],
        timestamp: Timestamp::from_unix_millis(7),
    }));
    let wire = encode_pi_messages(
        model.common.model_ref.model.as_str(),
        &context,
        &PiMessagesOptions {
            max_tokens: Some(100),
            session_id: Some("session-1".into()),
            tool_choice: Some(PiMessagesToolChoice::Auto),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        OrderedJsonWriter::stringify(&wire.into()).unwrap(),
        r#"{"model":"auto","context":{"messages":[{"role":"user","content":"Hello","timestamp":7}]},"options":{"maxTokens":100,"sessionId":"session-1","toolChoice":"auto"}}"#
    );
}

#[test]
fn wire_pi_messages_pi_exact() {
    // Architecture v2 part 2 §10.8; Pi basis: the complete captured
    // `api/pi-messages.ts` matrix, including lossless turn-two assembly after
    // persistence.
    for case in fixture::family_cases(env!("CARGO_MANIFEST_DIR"), "pi-messages") {
        assert_captured_pi_messages_case(&case);
    }
}

fn assert_captured_pi_messages_case(case: &Path) {
    let canonical = fixture::canonical(case);
    let model = captured_pi_messages_model(&canonical["model"]);
    let mut context = fixture::context(&canonical["context"], &model.common, PiMessages::API_ID);
    let actual = encode_captured_pi_messages(&model, &context, &canonical);
    let expected = fs::read(case.join("request-turn-1.body.json")).expect("turn-one fixture");
    assert_eq!(
        actual,
        expected,
        "turn-one pi-messages body mismatch for {}",
        case.file_name().unwrap().to_string_lossy()
    );

    let response = fs::read(case.join("response-turn-1.sse")).expect("response fixture");
    let assistant = decode_pi_messages_sse(
        &response,
        PiMessagesDecodeContext {
            message_id: MessageId::new("fixture-turn-one-assistant"),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
        },
    )
    .last()
    .and_then(AssistantEvent::terminal_message)
    .expect("terminal pi-messages fixture assistant")
    .clone();
    let persisted = serde_json::to_vec(&assistant).expect("persist pi-messages assistant");
    let assistant: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore pi-messages assistant");
    context.messages.push(Message::Assistant(assistant));
    fixture::append_messages(
        &mut context,
        &canonical["turnTwoAppend"],
        &model.common,
        PiMessages::API_ID,
    );
    let actual = encode_captured_pi_messages(&model, &context, &canonical);
    let expected = fs::read(case.join("request-turn-2.body.json")).expect("turn-two fixture");
    assert_eq!(
        actual,
        expected,
        "turn-two pi-messages body mismatch for {}",
        case.file_name().unwrap().to_string_lossy()
    );
}

fn captured_pi_messages_model(value: &serde_json::Value) -> ModelDescriptor {
    ModelDescriptor {
        common: fixture::common_model(value),
        api: ApiModelConfig::Custom(CustomApiModelConfig {
            api: ApiId::new(PiMessages::API_ID),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: Default::default(),
    }
}

fn encode_captured_pi_messages(
    model: &ModelDescriptor,
    context: &Context,
    canonical: &serde_json::Value,
) -> Vec<u8> {
    let ApiModelConfig::Custom(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<PiMessages> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let values = &canonical["options"];
    let options = if canonical["entrypoint"] == "streamSimple" {
        let simple = fixture::simple_options(values);
        let estimate = estimate_context_tokens(context).expect("pi-messages fixture estimate");
        PiMessages::lower_simple(
            SimpleLoweringContext {
                model: &typed,
                compat: &PiMessagesCompat,
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
            &PiMessagesSimplePatch::default(),
        )
        .expect("lower pi-messages fixture")
    } else {
        PiMessagesOptions {
            temperature: values
                .get("temperature")
                .and_then(serde_json::Value::as_f64)
                .map(|value| value as f32),
            max_tokens: values
                .get("maxTokens")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as u32),
            reasoning: values
                .get("reasoning")
                .and_then(serde_json::Value::as_str)
                .map(|reasoning| {
                    serde_json::from_value(serde_json::Value::String(reasoning.into()))
                        .expect("pi-messages reasoning")
                }),
            cache_retention: values
                .get("cacheRetention")
                .and_then(serde_json::Value::as_str)
                .map(fixture::cache_retention),
            session_id: values
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            ..Default::default()
        }
    };
    let wire = PiMessages::encode(
        EncodeContext {
            model: &typed,
            context,
            compat: &PiMessagesCompat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("encode pi-messages fixture");
    OrderedJsonWriter::to_vec(&wire.into()).expect("pi-messages fixture wire")
}

#[test]
fn pi_messages_stream_and_terminal_pi_exact() {
    // Pi basis: packages/ai/test/pi-messages.test.ts, “streams text and tool
    // calls and resolves the terminal message”.
    let body = concat!(
        "data: {\"type\":\"start\"}\n\n",
        "data: {\"type\":\"text_start\",\"contentIndex\":0}\n\n",
        "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Hello\"}\n\n",
        "data: {\"type\":\"text_end\",\"contentIndex\":0,\"content\":\"Hello\"}\n\n",
        "data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":10,\"output\":5,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":15,\"cost\":{\"total\":0.3}},\"responseId\":\"resp_1\"}\n\n"
    );
    let events = decode_pi_messages_sse(
        body.as_bytes(),
        PiMessagesDecodeContext {
            message_id: MessageId::new("message-1"),
            provider: ProviderId::new("radius"),
            requested_model: ModelId::new("auto"),
            timestamp: Timestamp::from_unix_millis(9),
        },
    );
    let message = events
        .iter()
        .find_map(AssistantEvent::terminal_message)
        .unwrap();
    assert_eq!(message.finish.reason, AssistantFinishReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.usage.total_tokens(), 15);
    assert_eq!(message.cost.as_ref().map(|cost| cost.micros), Some(300_000));
    assert!(matches!(
        message.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "Hello"
    ));
}

fn sse_decode_context(message_id: &str) -> PiMessagesDecodeContext {
    PiMessagesDecodeContext {
        message_id: MessageId::new(message_id),
        provider: ProviderId::new("radius"),
        requested_model: ModelId::new("auto"),
        timestamp: Timestamp::default(),
    }
}

#[test]
fn pi_messages_sse_crlf_normalization_pi_exact() {
    // Architecture v2 part 2 §10.1; Pi basis:
    // packages/ai/src/api/pi-messages.ts:266-298 normalizes CRLF before
    // finding the next `\n\n` record boundary.
    let body = concat!(
        "data: {\"type\":\"start\"}\r\n\r\n",
        "data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0}}\r\n\r\n"
    );
    let events = decode_pi_messages_sse(body.as_bytes(), sse_decode_context("crlf"));
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Finished { .. })
    ));
}

#[test]
fn pi_messages_sse_first_data_line_and_done_trim_pi_exact() {
    // Architecture v2 part 2 §10.1; Pi basis:
    // packages/ai/src/api/pi-messages.ts:300-310 selects the first `data:`
    // line and trims both ends, including whitespace after `[DONE]`.
    let body = concat!(
        "data: {\"type\":\"start\"}\n",
        "data: this second data line is ignored\n\n",
        "data: [DONE]   \n\n",
        "data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0}}\n\n"
    );
    let events = decode_pi_messages_sse(body.as_bytes(), sse_decode_context("first-data"));
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Finished { .. })
    ));
}

#[test]
fn pi_messages_sse_ecmascript_trim_pi_exact() {
    // Architecture v2 part 2 §10.1 and §10.8; Pi basis:
    // packages/ai/src/api/pi-messages.ts `parsePiMessagesEvent`, whose
    // JavaScript `trim()` includes U+FEFF but excludes U+0085.
    let start = r#"{"type":"start"}"#;
    let done = r#"{"type":"done","reason":"stop","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}"#;
    let body = format!("data:\u{feff}{start}\u{feff}\n\ndata:\u{feff}{done}\u{feff}\n\n");
    let events =
        decode_pi_messages_sse(body.as_bytes(), sse_decode_context("ecmascript-trim-feff"));
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Finished { .. })
    ));

    let body = format!("data:\u{0085}{start}\u{0085}\n\n");
    let events =
        decode_pi_messages_sse(body.as_bytes(), sse_decode_context("ecmascript-trim-u0085"));
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
            .is_some_and(|error| error.message.contains("invalid pi-messages SSE JSON"))
    );
}

#[test]
fn pi_messages_sse_lone_carriage_returns_are_not_boundaries_pi_exact() {
    // Architecture v2 part 2 §10.1; Pi basis:
    // packages/ai/src/api/pi-messages.ts:266-310 normalizes only CRLF and
    // therefore does not accept a lone `\r\r` as an SSE record boundary.
    let body = concat!(
        "data: {\"type\":\"start\"}\r\r",
        "data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0}}\n\n"
    );
    let events = decode_pi_messages_sse(body.as_bytes(), sse_decode_context("lone-cr"));
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("protocol failure terminal message");
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert!(
        message
            .finish
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("invalid pi-messages SSE JSON"))
    );
}

#[test]
fn pi_messages_missing_terminal_commits_failure() {
    // Pi basis: packages/ai/test/pi-messages.test.ts, “errors when the stream
    // ends without a terminal event”.
    let events = decode_pi_messages_sse(
        b"data: {\"type\":\"start\"}\n\n",
        PiMessagesDecodeContext {
            message_id: MessageId::new("message-1"),
            provider: ProviderId::new("radius"),
            requested_model: ModelId::new("auto"),
            timestamp: Timestamp::default(),
        },
    );
    assert!(matches!(events.last(), Some(AssistantEvent::Failed { .. })));
}

fn rewrite_terminal(kind: &str, reason: &str) -> Vec<AssistantEvent> {
    let body = format!(
        "data: {{\"type\":\"{kind}\",\"reason\":\"{reason}\",\"usage\":{{\"input\":1,\"output\":2,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":3}},\"errorMessage\":\"failed\",\"rewrite\":{{\"policyId\":\"policy-1\",\"policyVersion\":\"v2\",\"changed\":true,\"tokenCountChange\":-7,\"messageCountChange\":-1,\"systemPromptChanged\":true}}}}\n\n"
    );
    decode_pi_messages_sse(
        body.as_bytes(),
        PiMessagesDecodeContext {
            message_id: MessageId::new(format!("rewrite-{kind}")),
            provider: ProviderId::new("radius"),
            requested_model: ModelId::new("auto"),
            timestamp: Timestamp::default(),
        },
    )
}

#[test]
fn pi_messages_rewrite_diagnostic_survives_round_trip() {
    // Architecture v2 part 2 §2.1 and §10.2; Pi basis:
    // api/pi-messages.ts:165-173,191-207 appends the rewrite diagnostic on
    // both successful and failed terminal events.
    for events in [
        rewrite_terminal("done", "stop"),
        rewrite_terminal("error", "error"),
    ] {
        let message = events
            .last()
            .and_then(AssistantEvent::terminal_message)
            .expect("rewrite terminal message");
        let restored: AssistantMessage = serde_json::from_slice(
            &serde_json::to_vec(message).expect("persist rewrite diagnostic"),
        )
        .expect("restore rewrite diagnostic");
        let diagnostic = restored
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.kind == "pi_messages_rewrite")
            .expect("persisted pi_messages_rewrite diagnostic");
        assert_eq!(
            diagnostic.schema_version,
            ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION
        );
        assert_eq!(
            diagnostic.details["policyId"],
            serde_json::json!("policy-1")
        );
        assert_eq!(diagnostic.details["policyVersion"], serde_json::json!("v2"));
        assert_eq!(diagnostic.details["changed"], serde_json::json!(true));
        assert_eq!(
            diagnostic.details["tokenCountChange"],
            serde_json::json!(-7)
        );
        assert_eq!(
            diagnostic.details["messageCountChange"],
            serde_json::json!(-1)
        );
        assert_eq!(
            diagnostic.details["systemPromptChanged"],
            serde_json::json!(true)
        );
    }
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
                diagnostics: vec![transport_diagnostic("pi_messages_response_recovery")],
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new("stream_failure", "body failed")
                        .with_provider_code("pi_messages_body")
                        .with_status(503)
                        .with_request_id("request-body")
                        .with_diagnostic(transport_diagnostic("pi_messages_body_recovery")))
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
                diagnostics: vec![transport_diagnostic("pi_messages_response_recovery")],
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new("stream_failure", "body failed")
                        .with_provider_code("pi_messages_body")
                        .with_status(503)
                        .with_request_id("request-body")
                        .with_diagnostic(transport_diagnostic("pi_messages_body_recovery")))
                })),
            })
        })
    }
}

fn pi_messages_descriptor() -> ModelDescriptor {
    let model = typed_model();
    ModelDescriptor {
        common: model.common,
        api: ApiModelConfig::Custom(model.config),
        extensions: model.extensions,
    }
}

fn pi_messages_context() -> Context {
    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("user-transport"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("user-transport-text"),
            text: "Hello".into(),
        }],
        timestamp: Timestamp::default(),
    }));
    context
}

fn pi_messages_request(model: ModelDescriptor) -> ResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    ResolvedApiRequest {
        model,
        context: pi_messages_context(),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: Default::default(),
        endpoint,
        headers: http::HeaderMap::new(),
        auth_headers: http::HeaderMap::new(),
        api_key: None,
        api: ApiId::new(PiMessages::API_ID),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_pi_messages_request(model: ModelDescriptor) -> LocalResolvedApiRequest {
    let request = pi_messages_request(model);
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

fn assert_transport_diagnostics(message: &AssistantMessage) {
    let restored: AssistantMessage = serde_json::from_slice(
        &serde_json::to_vec(message).expect("persist pi-messages diagnostic message"),
    )
    .expect("restore pi-messages diagnostic message");
    assert_eq!(
        restored
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.kind.as_str())
            .collect::<Vec<_>>(),
        ["pi_messages_response_recovery", "pi_messages_body_recovery"]
    );
    let error = restored.finish.error.expect("transport failure");
    assert_eq!(error.code, "stream_failure");
    assert_eq!(error.provider_code.as_deref(), Some("pi_messages_body"));
    assert_eq!(error.status, Some(503));
    assert_eq!(error.request_id.as_deref(), Some("request-body"));
}

#[test]
fn pi_messages_transport_diagnostics_are_committed_send_and_local() {
    // Architecture v2 part 2 §2.1 and §9.2; Pi basis:
    // api/pi-messages.ts established-response stream commitment.
    let api = pi_messages_api(Arc::new(DiagnosticTransport));
    let mut stream = futures_executor::block_on(api.stream(
        pi_messages_request(pi_messages_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Send pi-messages stream");
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
            .expect("Send pi-messages terminal"),
    );

    let api = local_pi_messages_api(Rc::new(DiagnosticTransport));
    let mut stream = futures_executor::block_on(api.stream(
        local_pi_messages_request(pi_messages_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Local pi-messages stream");
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
            .expect("Local pi-messages terminal"),
    );
}

#[derive(Clone, Copy)]
struct NonSuccessTransport;

impl HttpTransport for NonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                401,
                http::HeaderMap::new(),
                br#"{"error":{"message":"Token expired","code":"unauthorized"}}"#.to_vec(),
            ))
        })
    }
}

#[derive(Clone)]
struct ObservedNonSuccessTransport {
    body_polled: Arc<AtomicBool>,
}

impl HttpTransport for ObservedNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let body_polled = Arc::clone(&self.body_polled);
        Box::pin(async move {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "x-pi-gateway-upstream-provider",
                "anthropic".parse().unwrap(),
            );
            Ok(HttpResponse {
                status: 401,
                headers,
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async move {
                    body_polled.store(true, Ordering::SeqCst);
                    Ok(br#"{"error":{"message":"Token expired","code":"unauthorized"}}"#.to_vec())
                })),
            })
        })
    }
}

impl LocalHttpTransport for ObservedNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let body_polled = Arc::clone(&self.body_polled);
        Box::pin(async move {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "x-pi-gateway-upstream-provider",
                "anthropic".parse().unwrap(),
            );
            Ok(LocalHttpResponse {
                status: 401,
                headers,
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async move {
                    body_polled.store(true, Ordering::SeqCst);
                    Ok(br#"{"error":{"message":"Token expired","code":"unauthorized"}}"#.to_vec())
                })),
            })
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedResponse {
    status: u16,
    body_was_unread: bool,
    upstream: Option<String>,
}

struct SendRawResponseObserver {
    body_polled: Arc<AtomicBool>,
    observed: Arc<Mutex<Vec<ObservedResponse>>>,
}

impl ResponseObserver for SendRawResponseObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        self.observed.lock().unwrap().push(ObservedResponse {
            status: response.status,
            body_was_unread: !self.body_polled.load(Ordering::SeqCst),
            upstream: response
                .headers
                .get("x-pi-gateway-upstream-provider")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        });
        Box::pin(async { Ok(()) })
    }
}

struct LocalRawResponseObserver {
    body_polled: Arc<AtomicBool>,
    observed: Arc<Mutex<Vec<ObservedResponse>>>,
}

impl LocalResponseObserver for LocalRawResponseObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        self.observed.lock().unwrap().push(ObservedResponse {
            status: response.status,
            body_was_unread: !self.body_polled.load(Ordering::SeqCst),
            upstream: response
                .headers
                .get("x-pi-gateway-upstream-provider")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        });
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn pi_messages_non_2xx_response_observer_runs_before_body_consumption_send_and_local() {
    // Architecture v2 part 2 §2.5, §2.6, §9.2, and §10.4
    // `response_observer_runs_before_body_consumption`; Pi basis:
    // packages/ai/src/api/pi-messages.ts:394-400 invokes onResponse with raw
    // status/headers before awaiting the error body.
    let body_polled = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let transport = ObservedNonSuccessTransport {
        body_polled: Arc::clone(&body_polled),
    };
    let mut request = pi_messages_request(pi_messages_descriptor());
    request.response_observers = Arc::from(vec![Arc::new(SendRawResponseObserver {
        body_polled: Arc::clone(&body_polled),
        observed: Arc::clone(&observed),
    }) as Arc<dyn ResponseObserver>]);
    let mut stream = futures_executor::block_on(
        pi_messages_api(Arc::new(transport)).stream(request, CancellationToken::new()),
    )
    .expect("Send non-2xx establishes a pi-messages stream");
    let send_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_response_failure(
        send_events
            .last()
            .and_then(AssistantEvent::terminal_message)
            .expect("Send response-failure terminal"),
    );
    assert!(body_polled.load(Ordering::SeqCst));
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        [ObservedResponse {
            status: 401,
            body_was_unread: true,
            upstream: Some("anthropic".into()),
        }]
    );

    body_polled.store(false, Ordering::SeqCst);
    observed.lock().unwrap().clear();
    let transport = ObservedNonSuccessTransport {
        body_polled: Arc::clone(&body_polled),
    };
    let mut request = local_pi_messages_request(pi_messages_descriptor());
    request.response_observers = Rc::from(vec![Rc::new(LocalRawResponseObserver {
        body_polled: Arc::clone(&body_polled),
        observed: Arc::clone(&observed),
    }) as Rc<dyn LocalResponseObserver>]);
    let mut stream = futures_executor::block_on(
        local_pi_messages_api(Rc::new(transport)).stream(request, CancellationToken::new()),
    )
    .expect("Local non-2xx establishes a pi-messages stream");
    let local_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_response_failure(
        local_events
            .last()
            .and_then(AssistantEvent::terminal_message)
            .expect("Local response-failure terminal"),
    );
    assert!(body_polled.load(Ordering::SeqCst));
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        [ObservedResponse {
            status: 401,
            body_was_unread: true,
            upstream: Some("anthropic".into()),
        }]
    );
}

#[derive(Clone)]
struct DebugCaptureTransport {
    urls: Arc<Mutex<Vec<String>>>,
}

fn successful_pi_messages_response() -> Vec<u8> {
    b"data: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":0}}\n\n".to_vec()
}

impl HttpTransport for DebugCaptureTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.urls.lock().unwrap().push(request.url.to_string());
        Box::pin(async {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "x-pi-gateway-upstream-provider",
                "anthropic".parse().unwrap(),
            );
            Ok(HttpResponse::from_bytes(
                200,
                headers,
                successful_pi_messages_response(),
            ))
        })
    }
}

impl LocalHttpTransport for DebugCaptureTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.urls.lock().unwrap().push(request.url.to_string());
        Box::pin(async {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "x-pi-gateway-upstream-provider",
                "anthropic".parse().unwrap(),
            );
            Ok(LocalHttpResponse::from_bytes(
                200,
                headers,
                successful_pi_messages_response(),
            ))
        })
    }
}

#[test]
fn pi_messages_debug_query_and_response_headers_send_and_local_pi_exact() {
    // Architecture v2 part 2 §2.5 and §9.2; Pi basis:
    // packages/ai/test/pi-messages.test.ts:160-180.
    let urls = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let transport = DebugCaptureTransport {
        urls: Arc::clone(&urls),
    };
    let mut request = pi_messages_request(pi_messages_descriptor());
    request.full_options = Some(ErasedApiFullOptions::new::<PiMessages>(PiMessagesOptions {
        debug: true,
        ..Default::default()
    }));
    request.response_observers = Arc::from(vec![Arc::new(SendRawResponseObserver {
        body_polled: Arc::new(AtomicBool::new(false)),
        observed: Arc::clone(&observed),
    }) as Arc<dyn ResponseObserver>]);
    futures_executor::block_on(
        pi_messages_api(Arc::new(transport.clone())).stream(request, CancellationToken::new()),
    )
    .expect("Send debug request");

    let mut request = local_pi_messages_request(pi_messages_descriptor());
    request.full_options = Some(ErasedApiFullOptions::new::<PiMessages>(PiMessagesOptions {
        debug: true,
        ..Default::default()
    }));
    request.response_observers = Rc::from(vec![Rc::new(LocalRawResponseObserver {
        body_polled: Arc::new(AtomicBool::new(false)),
        observed: Arc::clone(&observed),
    }) as Rc<dyn LocalResponseObserver>]);
    futures_executor::block_on(
        local_pi_messages_api(Rc::new(transport)).stream(request, CancellationToken::new()),
    )
    .expect("Local debug request");

    assert_eq!(
        urls.lock().unwrap().as_slice(),
        [
            "https://radius.pi.dev/v1/messages?debug=1",
            "https://radius.pi.dev/v1/messages?debug=1",
        ]
    );
    assert_eq!(observed.lock().unwrap().len(), 2);
    assert!(observed.lock().unwrap().iter().all(|response| {
        response.status == 200 && response.upstream.as_deref() == Some("anthropic")
    }));
}

#[derive(Clone, Copy)]
struct PendingNonSuccessTransport;

impl HttpTransport for PendingNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 401,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

impl LocalHttpTransport for PendingNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 401,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

#[test]
fn pi_messages_pending_non_2xx_body_is_cancellable_send_and_local() {
    // Architecture v2 part 2 §2.6, §9.2, and §9.5; Pi basis:
    // api/pi-messages.ts uses the request AbortSignal for response-body reads.
    let cancellation = CancellationToken::new();
    let mut stream = futures_executor::block_on(
        pi_messages_api(Arc::new(PendingNonSuccessTransport)).stream(
            pi_messages_request(pi_messages_descriptor()),
            cancellation.clone(),
        ),
    )
    .expect("Send pending failure stream establishes");
    let cancel = cancellation.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        cancel.cancel();
    });
    let events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    thread.join().unwrap();
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Cancelled { .. })
    ));

    let cancellation = CancellationToken::new();
    let mut stream = futures_executor::block_on(
        local_pi_messages_api(Rc::new(PendingNonSuccessTransport)).stream(
            local_pi_messages_request(pi_messages_descriptor()),
            cancellation.clone(),
        ),
    )
    .expect("Local pending failure stream establishes");
    let cancel = cancellation.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        cancel.cancel();
    });
    let events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    thread.join().unwrap();
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Cancelled { .. })
    ));
}

#[derive(Clone)]
struct OversizedNonSuccessTransport {
    body: Arc<Vec<u8>>,
}

impl HttpTransport for OversizedNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let body = self.body.as_ref().clone();
        Box::pin(async move { Ok(HttpResponse::from_bytes(401, http::HeaderMap::new(), body)) })
    }
}

impl LocalHttpTransport for OversizedNonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let body = self.body.as_ref().clone();
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                401,
                http::HeaderMap::new(),
                body,
            ))
        })
    }
}

fn terminal_error_message(events: &[AssistantEvent]) -> &str {
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .and_then(|message| message.finish.error.as_ref())
        .map(|error| error.message.as_str())
        .expect("response failure message")
}

#[test]
fn pi_messages_oversized_non_2xx_body_is_bounded_send_and_local() {
    // Architecture v2 part 2 §2.6 and §9.2; same 64 KiB bounded failure-body
    // contract as crates/pi-ai's generic provider error normalization.
    let mut body = vec![b'x'; MAX_PROVIDER_ERROR_BODY_BYTES + 1024];
    body.extend_from_slice(b"TAIL_MARKER");
    let transport = OversizedNonSuccessTransport {
        body: Arc::new(body),
    };
    let mut stream =
        futures_executor::block_on(pi_messages_api(Arc::new(transport.clone())).stream(
            pi_messages_request(pi_messages_descriptor()),
            CancellationToken::new(),
        ))
        .unwrap();
    let send_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    let message = terminal_error_message(&send_events);
    assert!(message.len() <= MAX_PROVIDER_ERROR_BODY_BYTES + 64);
    assert!(!message.contains("TAIL_MARKER"));

    let mut stream = futures_executor::block_on(local_pi_messages_api(Rc::new(transport)).stream(
        local_pi_messages_request(pi_messages_descriptor()),
        CancellationToken::new(),
    ))
    .unwrap();
    let local_events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    let message = terminal_error_message(&local_events);
    assert!(message.len() <= MAX_PROVIDER_ERROR_BODY_BYTES + 64);
    assert!(!message.contains("TAIL_MARKER"));
}

impl LocalHttpTransport for NonSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                401,
                http::HeaderMap::new(),
                br#"{"error":{"message":"Token expired","code":"unauthorized"}}"#.to_vec(),
            ))
        })
    }
}

fn assert_response_failure(message: &AssistantMessage) {
    let restored: AssistantMessage = serde_json::from_slice(
        &serde_json::to_vec(message).expect("persist response-failure assistant"),
    )
    .expect("restore response-failure assistant");
    assert_eq!(restored.finish.reason, AssistantFinishReason::Error);
    let error = restored.finish.error.expect("structured response failure");
    assert_eq!(error.code, "pi_messages_response_failure");
    assert_eq!(error.provider_code.as_deref(), Some("unauthorized"));
    assert_eq!(error.status, Some(401));
    assert!(error.message.contains("401 Unauthorized"));
    assert!(error.message.contains("Token expired"));
    assert!(error.message.contains("unauthorized"));
    let diagnostic = restored
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.kind == "pi_messages_response_failure")
        .expect("pi_messages_response_failure diagnostic");
    assert_eq!(diagnostic.details["status"], serde_json::json!(401));
    assert_eq!(diagnostic.details["provider"], serde_json::json!("radius"));
    assert_eq!(diagnostic.details["model"], serde_json::json!("auto"));
    assert_eq!(
        diagnostic.details["error"]["code"],
        serde_json::json!("unauthorized")
    );
}

mod send_response_failure {
    use super::*;

    #[test]
    fn pi_messages_non_2xx_commits_response_failure_diagnostic() {
        // Architecture v2 part 2 §2.1, §9.2, and §10.8; Pi basis:
        // packages/ai/test/pi-messages.test.ts “surfaces backend error
        // responses with diagnostics” and api/pi-messages.ts:85-152,313-335.
        let api = pi_messages_api(Arc::new(NonSuccessTransport));
        let mut stream = futures_executor::block_on(api.stream(
            pi_messages_request(pi_messages_descriptor()),
            CancellationToken::new(),
        ))
        .expect("non-2xx is an established Send assistant stream");
        let events = futures_executor::block_on(async {
            let mut events = Vec::new();
            while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
                events.push(event);
            }
            events
        });
        assert_response_failure(
            events
                .last()
                .and_then(AssistantEvent::terminal_message)
                .expect("Send response-failure terminal"),
        );
    }
}

mod local_response_failure {
    use super::*;

    #[test]
    fn pi_messages_non_2xx_commits_response_failure_diagnostic() {
        // Local §9.2 counterpart; same pinned Pi basis as the Send test.
        let api = local_pi_messages_api(Rc::new(NonSuccessTransport));
        let mut stream = futures_executor::block_on(api.stream(
            local_pi_messages_request(pi_messages_descriptor()),
            CancellationToken::new(),
        ))
        .expect("non-2xx is an established local assistant stream");
        let events = futures_executor::block_on(async {
            let mut events = Vec::new();
            while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
                events.push(event);
            }
            events
        });
        assert_response_failure(
            events
                .last()
                .and_then(AssistantEvent::terminal_message)
                .expect("local response-failure terminal"),
        );
    }
}

#[derive(Clone)]
struct PiMessagesBodyTransport {
    body: Arc<Vec<u8>>,
}

impl HttpTransport for PiMessagesBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let body = Arc::clone(&self.body);
        Box::pin(async move {
            Ok(HttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                body.as_ref().clone(),
            ))
        })
    }
}

impl LocalHttpTransport for PiMessagesBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let body = Arc::clone(&self.body);
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                200,
                http::HeaderMap::new(),
                body.as_ref().clone(),
            ))
        })
    }
}

fn authoritative_tool_call_body() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"start\"}\n\n",
        "data: {\"type\":\"toolcall_start\",\"contentIndex\":0,\"id\":\"provisional-id\",\"toolName\":\"provisional-name\"}\n\n",
        "data: {\"type\":\"toolcall_delta\",\"contentIndex\":0,\"delta\":\"{\\\"old\\\":true}\"}\n\n",
        "data: {\"type\":\"toolcall_end\",\"contentIndex\":0,\"toolCall\":{\"type\":\"toolCall\",\"id\":\"final-id\",\"name\":\"final-name\",\"arguments\":{\"final\":true}}}\n\n",
        "data: {\"type\":\"done\",\"reason\":\"toolUse\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2}}\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn assert_authoritative_tool_call(events: &[AssistantEvent]) {
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantEvent::ToolCallMetadataReplaced { call_id, name, .. }
            if call_id == &ToolCallId::new("final-id") && name == "final-name"
    )));
    let message = events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("pi-messages terminal");
    assert!(matches!(
        message.content.as_slice(),
        [ContentBlock::ToolCall { call, .. }]
            if call.id == ToolCallId::new("final-id")
                && call.name == "final-name"
                && call.arguments == serde_json::json!({"final": true})
    ));
}

#[test]
fn pi_messages_toolcall_end_authoritative_identity_send_and_local() {
    // Architecture v2 part 2 §1.3 and §9.2; Pi basis:
    // packages/ai/src/api/pi-messages.ts:251-258 Object.assigns the complete
    // terminal toolCall over the partial block.
    let transport = PiMessagesBodyTransport {
        body: Arc::new(authoritative_tool_call_body()),
    };
    let api = pi_messages_api(Arc::new(transport.clone()));
    let mut stream = futures_executor::block_on(api.stream(
        pi_messages_request(pi_messages_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Send pi-messages stream");
    let events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_authoritative_tool_call(&events);

    let api = local_pi_messages_api(Rc::new(transport));
    let mut stream = futures_executor::block_on(api.stream(
        local_pi_messages_request(pi_messages_descriptor()),
        CancellationToken::new(),
    ))
    .expect("Local pi-messages stream");
    let events = futures_executor::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        events
    });
    assert_authoritative_tool_call(&events);
}
