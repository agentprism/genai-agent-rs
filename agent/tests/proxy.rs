#![cfg(feature = "proxy")]

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    Binary, CacheControl, ChatMessage, ChatOptions, ChatResponseFormat, ContentPart, JsonSpec,
    MessageContent, MessageOptions, ReasoningEffort, ServiceTier, Tool, ToolCall, ToolChoice,
    ToolConfig, ToolName, ToolResponse, Verbosity, WebSearchConfig,
};
use genai::resolver::{AuthData, Endpoint};
use genai::{Headers, ModelIden, ModelSpec, ServiceTarget};
use rust_genai_agent::proxy::{
    ProxyAssistantMessageEvent, ProxyConfigError, ProxyDoneReason, ProxyErrorReason,
    ProxyRequestError, ProxyRequestV1, ProxyStreamFn, ProxyStreamOptions, ProxyToolCall,
    ProxyUsage, ProxyUsageCost, stream_proxy,
};
use rust_genai_agent::{
    AgentToolCall, AssistantContent, AssistantMessage, AssistantMessageEvent,
    AssistantMessageEventStream, LlmContext, OnPayloadHook, OnResponseHook, StopReason, StreamFn,
    StreamRequest, StreamResponseInfo,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// Generous deadlock safety-net (not a latency assertion): local capture-server round-trips finish
// in sub-millisecond time, so a tight bound only ever fires spuriously under CPU-saturated runs.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);
// Latency assertion (intentionally short): how long we wait to confirm NO request was sent.
const NO_REQUEST_WINDOW: Duration = Duration::from_millis(150);

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).expect("captured request body must be JSON")
    }
}

#[derive(Debug)]
struct ResponsePlan {
    chunks: Vec<Vec<u8>>,
    inter_chunk_delay: Option<Duration>,
    stall_after_chunks: bool,
}

impl ResponsePlan {
    fn one(response: Vec<u8>) -> Self {
        Self {
            chunks: vec![response],
            inter_chunk_delay: None,
            stall_after_chunks: false,
        }
    }

    fn every_byte(response: Vec<u8>) -> Self {
        Self {
            chunks: response.into_iter().map(|byte| vec![byte]).collect(),
            inter_chunk_delay: None,
            stall_after_chunks: false,
        }
    }

    fn stalling(response_prefix: Vec<u8>) -> Self {
        Self {
            chunks: vec![response_prefix],
            inter_chunk_delay: None,
            stall_after_chunks: true,
        }
    }
}

struct RawServer {
    base_url: String,
    captured: oneshot::Receiver<CapturedRequest>,
    task: JoinHandle<()>,
}

impl RawServer {
    async fn spawn(plan: ResponsePlan) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw test server");
        let address = listener.local_addr().expect("raw server local address");
        let (captured_tx, captured) = oneshot::channel();
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Ok(request) = read_request(&mut socket).await else {
                return;
            };
            let _ = captured_tx.send(request);

            for chunk in plan.chunks {
                if socket.write_all(&chunk).await.is_err() {
                    return;
                }
                if let Some(delay) = plan.inter_chunk_delay {
                    tokio::time::sleep(delay).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
            let _ = socket.flush().await;
            if plan.stall_after_chunks {
                std::future::pending::<()>().await;
            }
        });
        Self {
            base_url: format!("http://{address}"),
            captured,
            task,
        }
    }

    async fn request(&mut self) -> CapturedRequest {
        tokio::time::timeout(TEST_TIMEOUT, &mut self.captured)
            .await
            .expect("proxy did not reach the raw server before the bounded timeout")
            .expect("raw server stopped before capturing a request")
    }

    async fn assert_no_request(&mut self) {
        let outcome = tokio::time::timeout(NO_REQUEST_WINDOW, &mut self.captured).await;
        assert!(
            outcome.is_err(),
            "a ModelSpec::Target or pre-cancelled request must be rejected before HTTP"
        );
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    const MAX_REQUEST: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut scratch).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&scratch[..read]);
        if bytes.len() > MAX_REQUEST {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request exceeded test bound",
            ));
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let head = std::str::from_utf8(&bytes[..header_end - 4])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let target = request_parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while bytes.len() < header_end + content_length {
        let read = socket.read(&mut scratch).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended before declared body",
            ));
        }
        bytes.extend_from_slice(&scratch[..read]);
        if bytes.len() > MAX_REQUEST {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request exceeded test bound",
            ));
        }
    }

    Ok(CapturedRequest {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn http_response(status: &str, content_type: &str, body: impl AsRef<[u8]>) -> Vec<u8> {
    let body = body.as_ref();
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn open_sse_response(body_prefix: impl AsRef<[u8]>) -> Vec<u8> {
    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n".to_vec();
    response.extend_from_slice(body_prefix.as_ref());
    response
}

fn sse_body(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    let mut body = b": heartbeat\r\n\r\n".to_vec();
    for event in events {
        body.extend_from_slice(b"event: message\r\n");
        body.extend_from_slice(b"data: ");
        body.extend_from_slice(
            serde_json::to_string(&event)
                .expect("serialize SSE fixture")
                .as_bytes(),
        );
        body.extend_from_slice(b"\r\n\r\n");
    }
    body
}

fn success_sse() -> Vec<u8> {
    let body = sse_body([
        json!({"type": "start"}),
        json!({
            "type": "done",
            "reason": "stop",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0
            }
        }),
    ]);
    http_response("200 OK", "text/event-stream", body)
}

fn default_request(model: impl Into<ModelSpec>) -> StreamRequest {
    StreamRequest::new(
        model,
        LlmContext {
            system_prompt: "You are helpful".to_owned(),
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
        },
    )
}

async fn collect_stream(
    stream: AssistantMessageEventStream,
) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
    let result = stream.result_handle();
    let events = tokio::time::timeout(TEST_TIMEOUT, stream.collect::<Vec<_>>())
        .await
        .expect("assistant event stream did not finish before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("assistant result did not resolve before timeout")
        .expect("assistant stream ended without a terminal event");
    (events, message)
}

fn terminal_error(events: &[AssistantMessageEvent]) -> &AssistantMessage {
    match events.last().expect("at least one assistant event") {
        AssistantMessageEvent::Error { error, .. } => error,
        other => panic!("expected terminal Error, got {other:?}"),
    }
}

#[test]
fn proxy_options_validate_and_normalize_urls() {
    let cases = [
        ("http://localhost:8080", "http://localhost:8080/api/stream"),
        ("http://localhost:8080/", "http://localhost:8080/api/stream"),
        (
            "https://proxy.example.test/base",
            "https://proxy.example.test/base/api/stream",
        ),
        (
            "https://proxy.example.test/base/",
            "https://proxy.example.test/base/api/stream",
        ),
    ];
    for (base, expected) in cases {
        let options = ProxyStreamOptions::new(base, "secret-token").expect("valid proxy options");
        assert_eq!(options.endpoint().as_str(), expected);
    }

    assert!(matches!(
        ProxyStreamOptions::new("not a URL", "token"),
        Err(ProxyConfigError::InvalidUrl { .. })
    ));
    assert!(matches!(
        ProxyStreamOptions::new("ftp://proxy.example.test", "token"),
        Err(ProxyConfigError::UnsupportedScheme { .. })
    ));
    assert!(matches!(
        ProxyStreamOptions::new("https://proxy.example.test/path?q=1", "token"),
        Err(ProxyConfigError::QueryNotAllowed)
    ));
    assert!(matches!(
        ProxyStreamOptions::new("https://proxy.example.test/path#frag", "token"),
        Err(ProxyConfigError::FragmentNotAllowed)
    ));
}

#[test]
fn proxy_options_validate_auth_and_redact_debug() {
    assert!(matches!(
        ProxyStreamOptions::new("https://proxy.example.test", ""),
        Err(ProxyConfigError::EmptyAuthToken)
    ));
    assert!(matches!(
        ProxyStreamOptions::new("https://proxy.example.test", "  \t"),
        Err(ProxyConfigError::EmptyAuthToken)
    ));
    assert!(matches!(
        ProxyStreamOptions::new("https://proxy.example.test", "bad\r\ntoken"),
        Err(ProxyConfigError::InvalidAuthToken)
    ));

    let options = ProxyStreamOptions::new("https://proxy.example.test/root", "first-secret")
        .expect("valid options");
    let debug = format!("{options:?}");
    assert!(debug.contains("ProxyStreamOptions"));
    assert!(debug.contains("/root/api/stream"));
    assert!(debug.contains("REDACTED"));
    assert!(!debug.contains("first-secret"));

    let rotated = options
        .with_auth_token("second-secret")
        .expect("valid rotated token");
    let rotated_debug = format!("{rotated:?}");
    assert!(!rotated_debug.contains("first-secret"));
    assert!(!rotated_debug.contains("second-secret"));
}

#[test]
fn compact_proxy_events_use_ts_camel_case_and_round_trip() {
    let usage = ProxyUsage {
        input: 11,
        output: 7,
        cache_read: 3,
        cache_write: 2,
        total_tokens: 23,
        cost: Some(ProxyUsageCost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.03,
            cache_write: 0.04,
            total: 0.37,
        }),
    };
    let cases = vec![
        (ProxyAssistantMessageEvent::Start, json!({"type": "start"})),
        (
            ProxyAssistantMessageEvent::TextStart { content_index: 1 },
            json!({"type": "text_start", "contentIndex": 1}),
        ),
        (
            ProxyAssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "hé".to_owned(),
            },
            json!({"type": "text_delta", "contentIndex": 1, "delta": "hé"}),
        ),
        (
            ProxyAssistantMessageEvent::TextEnd {
                content_index: 1,
                content_signature: Some("text-sig".to_owned()),
            },
            json!({
                "type": "text_end",
                "contentIndex": 1,
                "contentSignature": "text-sig"
            }),
        ),
        (
            ProxyAssistantMessageEvent::ThinkingStart { content_index: 2 },
            json!({"type": "thinking_start", "contentIndex": 2}),
        ),
        (
            ProxyAssistantMessageEvent::ThinkingDelta {
                content_index: 2,
                delta: "plan".to_owned(),
            },
            json!({"type": "thinking_delta", "contentIndex": 2, "delta": "plan"}),
        ),
        (
            ProxyAssistantMessageEvent::ThinkingEnd {
                content_index: 2,
                content_signature: Some("thinking-sig".to_owned()),
            },
            json!({
                "type": "thinking_end",
                "contentIndex": 2,
                "contentSignature": "thinking-sig"
            }),
        ),
        (
            ProxyAssistantMessageEvent::ToolCallStart {
                content_index: 3,
                id: "call-1".to_owned(),
                tool_name: "weather".to_owned(),
            },
            json!({
                "type": "toolcall_start",
                "contentIndex": 3,
                "id": "call-1",
                "toolName": "weather"
            }),
        ),
        (
            ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 3,
                delta: "{\"city\":".to_owned(),
            },
            json!({
                "type": "toolcall_delta",
                "contentIndex": 3,
                "delta": "{\"city\":"
            }),
        ),
        (
            ProxyAssistantMessageEvent::ToolCallEnd {
                content_index: 3,
                thought_signatures: vec!["tool-sig".to_owned()],
                tool_call: Some(ProxyToolCall::ToolCall {
                    id: "call-1".to_owned(),
                    name: "weather".to_owned(),
                    arguments: json!({"city": "München"}),
                    thought_signature: None,
                    namespace: Some("dynamic_tools".to_owned()),
                }),
            },
            json!({
                "type": "toolcall_end",
                "contentIndex": 3,
                "thoughtSignatures": ["tool-sig"],
                "toolCall": {
                    "type": "toolCall",
                    "id": "call-1",
                    "name": "weather",
                    "arguments": {"city": "München"},
                    "namespace": "dynamic_tools"
                }
            }),
        ),
        (
            ProxyAssistantMessageEvent::Done {
                reason: ProxyDoneReason::ToolUse,
                usage: usage.clone(),
                response_id: Some("resp-1".to_owned()),
                provider_stop_reason: Some("tool_calls".to_owned()),
            },
            json!({
                "type": "done",
                "reason": "toolUse",
                "usage": {
                    "input": 11,
                    "output": 7,
                    "cacheRead": 3,
                    "cacheWrite": 2,
                    "totalTokens": 23,
                    "cost": {
                        "input": 0.1,
                        "output": 0.2,
                        "cacheRead": 0.03,
                        "cacheWrite": 0.04,
                        "total": 0.37
                    }
                },
                "responseId": "resp-1",
                "providerStopReason": "tool_calls"
            }),
        ),
        (
            ProxyAssistantMessageEvent::Error {
                reason: ProxyErrorReason::Aborted,
                error_message: Some("Request aborted by user".to_owned()),
                usage,
                response_id: None,
                provider_stop_reason: None,
            },
            json!({
                "type": "error",
                "reason": "aborted",
                "errorMessage": "Request aborted by user",
                "usage": {
                    "input": 11,
                    "output": 7,
                    "cacheRead": 3,
                    "cacheWrite": 2,
                    "totalTokens": 23,
                    "cost": {
                        "input": 0.1,
                        "output": 0.2,
                        "cacheRead": 0.03,
                        "cacheWrite": 0.04,
                        "total": 0.37
                    }
                }
            }),
        ),
    ];

    for (event, golden) in cases {
        assert_eq!(
            serde_json::to_value(&event).expect("serialize event"),
            golden
        );
        let decoded: ProxyAssistantMessageEvent =
            serde_json::from_value(golden).expect("deserialize event");
        assert_eq!(decoded, event);
    }

    let negative_usage = json!({
        "type": "done",
        "reason": "stop",
        "usage": {
            "input": -1,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 0
        }
    });
    assert!(serde_json::from_value::<ProxyAssistantMessageEvent>(negative_usage).is_err());
}

#[tokio::test]
async fn preserves_tool_call_metadata_received_only_on_toolcall_end() {
    // Parity case: pi-agent-core `test/proxy.test.ts` "preserves tool-call metadata received
    // only on toolcall_end". A server may deliver the finalized tool call (id, name, parsed
    // arguments, and the pi-ai `namespace`) only with the closing wire event; the client merges
    // it onto the open tool-call block (TS `Object.assign(content, proxyEvent.toolCall)`).
    let events = vec![
        json!({ "type": "start" }),
        json!({
            "type": "toolcall_start",
            "contentIndex": 0,
            "id": "call_test|fc_test",
            "toolName": "lookup"
        }),
        json!({ "type": "toolcall_delta", "contentIndex": 0, "delta": "{\"value\":\"hello\"}" }),
        json!({
            "type": "toolcall_end",
            "contentIndex": 0,
            "toolCall": {
                "type": "toolCall",
                "id": "call_test|fc_test",
                "name": "lookup",
                "arguments": { "value": "hello" },
                "namespace": "dynamic_tools"
            }
        }),
        json!({
            "type": "done",
            "reason": "toolUse",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
            }
        }),
    ];
    let response = http_response("200 OK", "text/event-stream", sse_body(events));
    let server = RawServer::spawn(ResponsePlan::every_byte(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let request = default_request(ModelIden::new(AdapterKind::OpenAI, "gpt-5.4"));

    let (events, result) = collect_stream(stream_proxy(request, options).await).await;
    let end_event = events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
            _ => None,
        })
        .expect("toolcall_end event");
    assert_eq!(end_event.namespace.as_deref(), Some("dynamic_tools"));
    assert_eq!(end_event.arguments, json!({ "value": "hello" }));

    let Some(AssistantContent::ToolCall(content)) = result.content.first() else {
        panic!("expected a tool call as the first content block");
    };
    assert_eq!(content.arguments, json!({ "value": "hello" }));
    assert_eq!(content.namespace.as_deref(), Some("dynamic_tools"));
}

#[test]
fn name_request_wire_golden_is_explicit_and_omits_empty_values() {
    let request = StreamRequest::new(
        "openai::gpt-4o",
        LlmContext {
            system_prompt: "Be concise".to_owned(),
            messages: vec![ChatMessage::user(MessageContent::from_parts(vec![
                ContentPart::Text("hello".to_owned()),
                ContentPart::Binary(Binary::from_url(
                    "image/png",
                    "https://assets.example.test/a.png",
                    Some("a.png".to_owned()),
                )),
            ]))],
            tools: Vec::new(),
        },
    );

    let wire = ProxyRequestV1::try_from(&request).expect("Name model is proxy-safe");
    assert_eq!(
        serde_json::to_value(wire).expect("serialize stable request DTO"),
        json!({
            "model": {"type": "name", "name": "openai::gpt-4o"},
            "context": {
                "systemPrompt": "Be concise",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "hello"},
                        {
                            "type": "binary",
                            "contentType": "image/png",
                            "source": {
                                "type": "url",
                                "url": "https://assets.example.test/a.png"
                            },
                            "name": "a.png"
                        }
                    ]
                }]
            },
            "options": {}
        })
    );
}

#[test]
fn identity_request_wire_golden_tags_messages_content_and_tools() {
    let assistant = ChatMessage::assistant(MessageContent::from_parts(vec![
        ContentPart::ReasoningContent("plan".to_owned()),
        ContentPart::ThoughtSignature("sig-a".to_owned()),
        ContentPart::ToolCall(ToolCall {
            call_id: "call-7".to_owned(),
            fn_name: "lookup".to_owned(),
            fn_arguments: json!({"q": "rust"}),
            thought_signatures: Some(vec!["sig-tool".to_owned()]),
        }),
        ContentPart::Custom(genai::chat::CustomPart {
            model_iden: Some(ModelIden::new(AdapterKind::Anthropic, "claude-custom")),
            data: json!({"vendor": true}),
        }),
    ]))
    .with_options(MessageOptions::default().with_cache_control(CacheControl::Memory));
    let tool_response =
        ChatMessage::tool(ToolResponse::new("call-7", "result").with_fn_name("lookup"));
    let custom_tool = Tool::new(ToolName::Custom("lookup".to_owned()))
        .with_description("Find a record")
        .with_schema(json!({"type": "object"}))
        .with_config(ToolConfig::Custom(json!({"tenant": "a"})))
        .with_strict(true)
        .with_cache_control(CacheControl::Ephemeral5m)
        .with_eager_input_streaming(true);
    let web_tool = Tool::new_web_search().with_config(WebSearchConfig::default().with_max_uses(2));
    let request = StreamRequest::new(
        ModelIden::new(AdapterKind::Custom(7), "model-x"),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![assistant, tool_response],
            tools: vec![custom_tool, web_tool],
        },
    );

    let wire = ProxyRequestV1::try_from(&request).expect("Iden model is proxy-safe");
    assert_eq!(
        serde_json::to_value(wire).expect("serialize stable request DTO"),
        json!({
            "model": {"type": "identity", "adapter": "genai_7", "model": "model-x"},
            "context": {
                "messages": [
                    {
                        "role": "assistant",
                        "content": [
                            {"type": "reasoning", "reasoning": "plan"},
                            {"type": "thoughtSignature", "signature": "sig-a"},
                            {
                                "type": "toolCall",
                                "id": "call-7",
                                "name": "lookup",
                                "arguments": {"q": "rust"},
                                "thoughtSignatures": ["sig-tool"]
                            },
                            {
                                "type": "custom",
                                "data": {"vendor": true},
                                "model": {"adapter": "anthropic", "model": "claude-custom"}
                            }
                        ],
                        "options": {"cacheControl": "memory"}
                    },
                    {
                        "role": "tool",
                        "content": [{
                            "type": "toolResponse",
                            "id": "call-7",
                            "name": "lookup",
                            "content": "result"
                        }]
                    }
                ],
                "tools": [
                    {
                        "name": {"type": "custom", "name": "lookup"},
                        "description": "Find a record",
                        "schema": {"type": "object"},
                        "strict": true,
                        "config": {"type": "custom", "value": {"tenant": "a"}},
                        "cacheControl": "ephemeral5m",
                        "eagerInputStreaming": true
                    },
                    {
                        "name": {"type": "webSearch"},
                        "config": {"type": "webSearch", "maxUses": 2}
                    }
                ]
            },
            "options": {}
        })
    );
}

#[test]
fn non_finite_sampling_values_are_request_errors() {
    let mut request = default_request("gpt-4o");
    request.options.temperature = Some(f64::NAN);
    assert!(matches!(
        ProxyRequestV1::try_from(&request),
        Err(ProxyRequestError::NonFiniteOption {
            field: "temperature"
        })
    ));

    request.options.temperature = Some(0.2);
    request.options.top_p = Some(f64::INFINITY);
    assert!(matches!(
        ProxyRequestV1::try_from(&request),
        Err(ProxyRequestError::NonFiniteOption { field: "topP" })
    ));
}

#[tokio::test]
async fn target_model_is_rejected_in_band_before_any_http_request() {
    let mut server = RawServer::spawn(ResponsePlan::one(success_sse())).await;
    let target_model = ModelIden::new(AdapterKind::OpenAI, "private-target-model");
    let target = ServiceTarget {
        endpoint: Endpoint::from_static("https://upstream.internal/v1"),
        auth: AuthData::from_single("upstream-secret-that-must-not-leak"),
        model: target_model.clone(),
    };
    let request = default_request(ModelSpec::from_target(target));
    assert!(matches!(
        ProxyRequestV1::try_from(&request),
        Err(ProxyRequestError::TargetModelUnsupported)
    ));
    let options = ProxyStreamOptions::new(&server.base_url, "proxy-token").expect("proxy options");

    let (events, result) = collect_stream(stream_proxy(request, options).await).await;
    assert!(matches!(
        events.first(),
        Some(AssistantMessageEvent::Start { .. })
    ));
    assert_eq!(events.len(), 2);
    assert_eq!(result.model, target_model);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("ModelSpec::Target cannot be sent through the proxy")
    );
    server.assert_no_request().await;
}

#[tokio::test]
async fn proxy_stream_fn_posts_exact_safe_request_and_headers() {
    let mut server = RawServer::spawn(ResponsePlan::one(success_sse())).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("injected reqwest client");
    let options = ProxyStreamOptions::new(format!("{}/tenant/", server.base_url), "proxy-secret")
        .expect("proxy options")
        .with_client(client);

    let messages = vec![
        ChatMessage::system("message-level system"),
        ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::Text("hello".to_owned()),
            ContentPart::Binary(Binary::from_base64(
                "image/png",
                "YWJj",
                Some("tiny.png".to_owned()),
            )),
        ])),
        ChatMessage::assistant(MessageContent::from_parts(vec![
            ContentPart::ReasoningContent("think".to_owned()),
            ContentPart::ThoughtSignature("sig".to_owned()),
        ])),
    ];
    let tools = vec![
        Tool::new("lookup")
            .with_description("Lookup")
            .with_schema(json!({"type": "object", "properties": {}}))
            .with_custom_format(json!({"type": "grammar", "syntax": "lark"}))
            .with_strict(true),
    ];
    let mut chat_options = ChatOptions::default()
        .with_temperature(0.2)
        .with_max_tokens(1024)
        .with_top_p(0.9)
        .with_stop_sequences(vec!["END".to_owned()])
        .with_response_format(ChatResponseFormat::JsonSpec(
            JsonSpec::new("answer", json!({"type": "object"})).with_description("An answer"),
        ))
        .with_tool_choice(ToolChoice::tool("lookup"))
        .with_normalize_reasoning_content(true)
        .with_reasoning_effort(ReasoningEffort::Budget(4096))
        .with_verbosity(Verbosity::High)
        .with_seed(42)
        .with_service_tier(ServiceTier::Flex)
        .with_extra_headers(Headers::from([("x-tenant", "a"), ("x-trace", "b")]))
        .with_cache_control(CacheControl::Ephemeral1h)
        .with_prompt_cache_key("cache-key")
        .with_extra_body(json!({"top_k": 40}));
    chat_options.capture_usage = Some(true);
    chat_options.capture_content = Some(true);
    chat_options.capture_reasoning_content = Some(true);
    chat_options.capture_tool_calls = Some(true);
    chat_options.capture_raw_body = Some(true);
    let request = StreamRequest::new(
        "openai::gpt-4o",
        LlmContext {
            system_prompt: "You are helpful".to_owned(),
            messages,
            tools,
        },
    )
    .with_options(chat_options);

    let stream_fn = ProxyStreamFn::from_options(options);
    let _ = collect_stream(stream_fn.stream(request).await).await;
    let captured = server.request().await;

    assert_eq!(captured.method, "POST");
    assert_eq!(captured.target, "/tenant/api/stream");
    assert_eq!(
        captured.headers.get("authorization").map(String::as_str),
        Some("Bearer proxy-secret")
    );
    assert_eq!(
        captured.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        captured.json_body(),
        json!({
            "model": {"type": "name", "name": "openai::gpt-4o"},
            "context": {
                "systemPrompt": "You are helpful",
                "messages": [
                    {
                        "role": "system",
                        "content": [{"type": "text", "text": "message-level system"}]
                    },
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "hello"},
                            {
                                "type": "binary",
                                "contentType": "image/png",
                                "source": {"type": "base64", "data": "YWJj"},
                                "name": "tiny.png"
                            }
                        ]
                    },
                    {
                        "role": "assistant",
                        "content": [
                            {"type": "reasoning", "reasoning": "think"},
                            {"type": "thoughtSignature", "signature": "sig"}
                        ]
                    }
                ],
                "tools": [{
                    "name": {"type": "custom", "name": "lookup"},
                    "description": "Lookup",
                    "schema": {"type": "object", "properties": {}},
                    "customFormat": {"type": "grammar", "syntax": "lark"},
                    "strict": true
                }]
            },
            "options": {
                "temperature": 0.2,
                "maxTokens": 1024,
                "topP": 0.9,
                "stopSequences": ["END"],
                "responseFormat": {
                    "type": "jsonSpec",
                    "name": "answer",
                    "description": "An answer",
                    "schema": {"type": "object"}
                },
                "toolChoice": {"type": "tool", "name": "lookup"},
                "normalizeReasoningContent": true,
                "reasoningEffort": {"type": "budget", "tokens": 4096},
                "verbosity": "high",
                "seed": 42,
                "serviceTier": "flex",
                "extraHeaders": {"x-tenant": "a", "x-trace": "b"},
                "cacheControl": "ephemeral1h",
                "promptCacheKey": "cache-key",
                "extraBody": {"top_k": 40}
            }
        })
    );
    let raw_body = String::from_utf8(captured.body).expect("JSON body is UTF-8");
    for forbidden in [
        "proxy-secret",
        "authToken",
        "cancel",
        "captureUsage",
        "captureContent",
        "captureReasoningContent",
        "captureToolCalls",
        "captureRawBody",
    ] {
        assert!(
            !raw_body.contains(forbidden),
            "body leaked forbidden field {forbidden}"
        );
    }
}

#[tokio::test]
async fn byte_fragmented_sse_reconstructs_text_thinking_tools_signatures_and_usage() {
    let usage = json!({
        "input": 9,
        "output": 6,
        "cacheRead": 2,
        "cacheWrite": 1,
        "totalTokens": 18,
        "cost": {
            "input": 0.1,
            "output": 0.2,
            "cacheRead": 0.01,
            "cacheWrite": 0.02,
            "total": 0.33
        }
    });
    let events = [
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "hé"}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "llo"}),
        json!({"type": "text_end", "contentIndex": 0, "contentSignature": "text-sig"}),
        json!({"type": "thinking_start", "contentIndex": 1}),
        json!({"type": "thinking_delta", "contentIndex": 1, "delta": "pl"}),
        json!({"type": "thinking_delta", "contentIndex": 1, "delta": "an"}),
        json!({"type": "thinking_end", "contentIndex": 1, "contentSignature": "think-sig"}),
        json!({
            "type": "toolcall_start",
            "contentIndex": 2,
            "id": "call-1",
            "toolName": "weather"
        }),
        json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "{\"city\":\"Mü"}),
        json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "nchen\",\"nested\":{\"ok\":tr"}),
        json!({"type": "toolcall_delta", "contentIndex": 2, "delta": "ue}}"}),
        json!({
            "type": "toolcall_end",
            "contentIndex": 2,
            "thoughtSignatures": ["tool-sig-a", "tool-sig-b"]
        }),
        json!({
            "type": "done",
            "reason": "toolUse",
            "usage": usage,
            "responseId": "resp-9",
            "providerStopReason": "tool_calls"
        }),
    ];
    let response = http_response("200 OK", "text/event-stream", sse_body(events));
    let server = RawServer::spawn(ResponsePlan::every_byte(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let request = default_request(ModelIden::new(AdapterKind::OpenAI, "gpt-4o"));

    let (events, result) = collect_stream(stream_proxy(request, options).await).await;
    let first_tool_delta = events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallDelta { partial, .. } => Some(partial),
            _ => None,
        })
        .expect("first tool delta event");
    assert_eq!(
        first_tool_delta
            .tool_calls()
            .next()
            .map(|call| &call.arguments),
        Some(&json!({"city": "Mü"}))
    );
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert_eq!(result.response_id.as_deref(), Some("resp-9"));
    assert_eq!(result.provider_stop_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        result.usage,
        rust_genai_agent::AgentUsage::new(9, 6)
            .with_cache_read_tokens(2)
            .with_cache_write_tokens(1)
            .with_total_tokens(18)
    );
    assert_eq!(
        result.content,
        vec![
            AssistantContent::Text {
                text: "héllo".to_owned(),
                signature: Some("text-sig".to_owned()),
            },
            AssistantContent::Thinking {
                thinking: "plan".to_owned(),
                signature: Some("think-sig".to_owned()),
            },
            AssistantContent::ToolCall(
                AgentToolCall::new(
                    "call-1",
                    "weather",
                    json!({"city": "München", "nested": {"ok": true}}),
                )
                .with_thought_signatures(vec!["tool-sig-a".to_owned(), "tool-sig-b".to_owned()]),
            ),
        ]
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
}

#[tokio::test]
async fn http_json_error_uses_server_override_in_band() {
    let response = http_response(
        "429 Too Many Requests",
        "application/json",
        br#"{"error":"quota exhausted"}"#,
    );
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (events, result) =
        collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    assert!(matches!(
        events.first(),
        Some(AssistantMessageEvent::Start { .. })
    ));
    assert_eq!(events.len(), 2);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy error: quota exhausted")
    );
}

#[tokio::test]
async fn http_non_json_error_uses_status_fallback_without_echoing_body() {
    let response = http_response(
        "503 Service Unavailable",
        "text/plain",
        b"upstream-secret-body-must-not-be-echoed",
    );
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (_, result) = collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy error: 503 Service Unavailable")
    );
    assert!(
        !result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("secret-body")
    );
}

#[tokio::test]
async fn malformed_sse_json_is_an_in_band_error_preserving_partial_content() {
    let mut body = sse_body([
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "kept"}),
    ]);
    body.extend_from_slice(b"data: {not-json}\n\n");
    let response = http_response("200 OK", "text/event-stream", body);
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (events, result) =
        collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.text(), "kept");
    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("proxy event JSON")
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
}

#[tokio::test]
async fn out_of_order_delta_is_an_in_band_protocol_error_without_panicking() {
    let response = http_response(
        "200 OK",
        "text/event-stream",
        sse_body([
            json!({"type": "start"}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "bad"}),
        ]),
    );
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (events, result) =
        collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains("text_delta")
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
}

#[tokio::test]
async fn eof_without_terminal_becomes_a_resolving_in_band_error() {
    let response = http_response(
        "200 OK",
        "text/event-stream",
        sse_body([
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"}),
            json!({"type": "text_end", "contentIndex": 0}),
        ]),
    );
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (events, result) =
        collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.text(), "partial");
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy SSE stream ended without a terminal event")
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
}

#[tokio::test]
async fn cancellation_before_http_is_aborted_once_and_sends_no_request() {
    let mut server = RawServer::spawn(ResponsePlan::one(success_sse())).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let request = default_request("gpt-4o").with_cancellation(cancel);
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");

    let (events, result) = collect_stream(stream_proxy(request, options).await).await;
    assert_eq!(events.len(), 2);
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Request aborted by user")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::Error { .. }))
            .count(),
        1
    );
    server.assert_no_request().await;
}

#[tokio::test]
async fn cancellation_mid_stream_retains_accumulated_content() {
    let partial = sse_body([
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "retained"}),
    ]);
    let response = open_sse_response(partial);
    let server = RawServer::spawn(ResponsePlan::stalling(response)).await;
    let cancel = CancellationToken::new();
    let request = default_request("gpt-4o").with_cancellation(cancel.clone());
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(75)).await;
        cancel.cancel();
    });

    let (events, result) = collect_stream(stream_proxy(request, options).await).await;
    cancel_task.await.expect("cancellation task");
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Request aborted by user")
    );
    assert_eq!(result.text(), "retained");
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
}

#[tokio::test]
async fn first_terminal_wins_and_stream_is_fused() {
    let response = http_response(
        "200 OK",
        "text/event-stream",
        sse_body([
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "ok"}),
            json!({"type": "text_end", "contentIndex": 0}),
            json!({
                "type": "done",
                "reason": "stop",
                "usage": {
                    "input": 1,
                    "output": 1,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 2
                }
            }),
            json!({
                "type": "error",
                "reason": "error",
                "errorMessage": "must be ignored",
                "usage": {
                    "input": 99,
                    "output": 99,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 198
                }
            }),
        ]),
    );
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let mut stream = stream_proxy(default_request("gpt-4o"), options).await;
    let result = stream.result_handle();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(TEST_TIMEOUT, stream.next())
        .await
        .expect("stream poll timeout")
    {
        events.push(event);
    }
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
    let result = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result timeout")
        .expect("terminal result");

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.text(), "ok");
    assert_eq!(result.usage.total_tokens, 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
}

type SeenPayloads = Arc<Mutex<Vec<(Value, ModelIden)>>>;
type SeenResponses = Arc<Mutex<Vec<(StreamResponseInfo, ModelIden)>>>;

fn recording_on_payload(seen: SeenPayloads, replacement: Option<Value>) -> OnPayloadHook {
    Arc::new(move |payload, model| {
        let seen = seen.clone();
        let replacement = replacement.clone();
        Box::pin(async move {
            seen.lock().unwrap().push((payload, model));
            replacement
        })
    })
}

fn recording_on_response(seen: SeenResponses) -> OnResponseHook {
    Arc::new(move |info, model| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push((info, model));
        })
    })
}

#[tokio::test]
async fn on_payload_replacement_is_what_goes_over_the_wire() {
    let mut server = RawServer::spawn(ResponsePlan::one(success_sse())).await;
    let options = ProxyStreamOptions::new(&server.base_url, "proxy-secret").expect("proxy options");
    let seen: SeenPayloads = Arc::default();
    let replacement = json!({"model": {"type": "name", "name": "gpt-4o"}, "x_intercepted": true});
    let request = default_request("gpt-4o").with_on_payload(recording_on_payload(
        seen.clone(),
        Some(replacement.clone()),
    ));

    let (_, result) = collect_stream(stream_proxy(request, options).await).await;
    assert_eq!(result.stop_reason, StopReason::Stop);

    // The hook saw the original serialized wire body plus the model identity, never the token.
    {
        let payloads = seen.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        let (payload, model) = &payloads[0];
        assert_eq!(model, &ModelIden::new(AdapterKind::OpenAI, "gpt-4o"));
        assert_eq!(
            payload.pointer("/model/name").and_then(Value::as_str),
            Some("gpt-4o")
        );
        assert_eq!(
            payload
                .pointer("/context/messages/0/content/0/text")
                .and_then(Value::as_str),
            Some("hello")
        );
        assert!(!payload.to_string().contains("proxy-secret"));
    }

    // The replacement is exactly what was posted; the original context is gone from the wire.
    let captured = server.request().await;
    assert_eq!(captured.json_body(), replacement);
    let raw_body = String::from_utf8(captured.body).expect("JSON body is UTF-8");
    assert!(!raw_body.contains("hello"));
    assert!(!raw_body.contains("proxy-secret"));
    assert_eq!(
        captured.headers.get("authorization").map(String::as_str),
        Some("Bearer proxy-secret")
    );
}

#[tokio::test]
async fn on_payload_returning_none_keeps_the_original_wire_body() {
    let mut server = RawServer::spawn(ResponsePlan::one(success_sse())).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let seen: SeenPayloads = Arc::default();
    let request =
        default_request("gpt-4o").with_on_payload(recording_on_payload(seen.clone(), None));

    let (_, result) = collect_stream(stream_proxy(request, options).await).await;
    assert_eq!(result.stop_reason, StopReason::Stop);

    assert_eq!(seen.lock().unwrap().len(), 1);
    let captured = server.request().await;
    assert_eq!(
        captured.json_body(),
        json!({
            "model": {"type": "name", "name": "gpt-4o"},
            "context": {
                "systemPrompt": "You are helpful",
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}]
                }]
            },
            "options": {}
        })
    );
}

#[tokio::test]
async fn on_response_observes_status_and_headers_on_success_before_the_sse_body() {
    let body = sse_body([
        json!({"type": "start"}),
        json!({
            "type": "done",
            "reason": "stop",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0
            }
        }),
    ]);
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Obs-Test: obs-value\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let seen: SeenResponses = Arc::default();
    let request = default_request("gpt-4o").with_on_response(recording_on_response(seen.clone()));

    let (_, result) = collect_stream(stream_proxy(request, options).await).await;
    assert_eq!(result.stop_reason, StopReason::Stop);

    let responses = seen.lock().unwrap();
    assert_eq!(responses.len(), 1);
    let (info, model) = &responses[0];
    assert_eq!(info.status, 200);
    assert_eq!(info.header("x-obs-test"), Some("obs-value"));
    assert_eq!(model, &ModelIden::new(AdapterKind::OpenAI, "gpt-4o"));
}

#[tokio::test]
async fn on_response_observes_status_and_headers_on_a_non_ok_response() {
    let error_body = br#"{"error":"quota exhausted"}"#;
    let mut response = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        error_body.len()
    )
    .into_bytes();
    response.extend_from_slice(error_body);
    let server = RawServer::spawn(ResponsePlan::one(response)).await;
    let options = ProxyStreamOptions::new(&server.base_url, "token").expect("proxy options");
    let seen: SeenResponses = Arc::default();
    let request = default_request("gpt-4o").with_on_response(recording_on_response(seen.clone()));

    let (_, result) = collect_stream(stream_proxy(request, options).await).await;

    // The observer fired on the failing response head: status plus headers, no body.
    let responses = seen.lock().unwrap();
    assert_eq!(responses.len(), 1);
    let (info, _) = &responses[0];
    assert_eq!(info.status, 429);
    assert_eq!(info.header("retry-after"), Some("2"));
    assert!(
        !format!("{info:?}").contains("quota exhausted"),
        "the observer must never receive body content"
    );
    drop(responses);

    // The in-band error resolution afterwards is unchanged.
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy error: quota exhausted")
    );
}

#[tokio::test]
async fn connection_failures_are_returned_in_band() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve local port");
    let address = listener.local_addr().expect("reserved address");
    drop(listener);
    let options =
        ProxyStreamOptions::new(format!("http://{address}"), "token").expect("proxy options");

    let (events, result) =
        collect_stream(stream_proxy(default_request("gpt-4o"), options).await).await;
    let error = terminal_error(&events);
    assert_eq!(result, *error);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_ne!(
        result.error_message.as_deref(),
        Some("Proxy HTTP/SSE transport is not implemented (M6 T0 skeleton)")
    );
    assert!(
        result
            .error_message
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("connect")
    );
}
