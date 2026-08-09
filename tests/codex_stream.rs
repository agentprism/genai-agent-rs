//! Offline integration tests for [`CodexStreamFn`] against local mock servers.
//!
//! NO connection to chatgpt.com is made. These validate protocol framing,
//! headers, request-body fields, event → `AssistantMessageEvent` mapping,
//! WebSocket→SSE fallback, non-2xx handshake handling, and cancellation. See the
//! README/NOTES: end-to-end validation against the real ChatGPT backend is
//! intentionally out of scope for CI (needs a subscription, live OAuth, network).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use genai::ModelSpec;
use genai::chat::ChatMessage;
use rust_genai_agent::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, LlmContext, StopReason, StreamFn,
    StreamRequest, Transport,
};
use rust_genai_codex::{CodexStreamFn, StaticTokenSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Mock server helpers
// ============================================================================

#[derive(Clone, Debug, Default)]
struct CapturedHttp {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP/1.1 request (request line + headers + Content-Length body).
async fn read_http_request(stream: &mut TcpStream) -> CapturedHttp {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    let mut body = buf[(header_end + 4).min(buf.len())..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await.expect("read body");
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    CapturedHttp {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    }
}

fn sse_body(events: &[String]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(event);
        out.push_str("\n\n");
    }
    out
}

async fn write_sse_ok(stream: &mut TcpStream, events: &[String]) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}",
        sse_body(events)
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// SSE server: accept one request, capture it, serve the given events, close.
async fn spawn_sse_server(
    events: Vec<String>,
    capture: Arc<Mutex<Option<CapturedHttp>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let captured = read_http_request(&mut stream).await;
        *capture.lock().unwrap() = Some(captured);
        write_sse_ok(&mut stream, &events).await;
    });
    format!("http://{addr}")
}

/// SSE server that sends `prefix` events, then stalls (no terminal) so the
/// client can be cancelled mid-stream.
async fn spawn_stalling_sse_server(prefix: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_http_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}",
            sse_body(&prefix)
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
        // Hold the connection open (no terminal event) so the client stays
        // mid-stream until it cancels.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    format!("http://{addr}")
}

/// Server that returns a fixed non-2xx status + body for one request.
async fn spawn_status_server(status_line: &'static str, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_http_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    });
    format!("http://{addr}")
}

/// WebSocket server: accept one WS connection, capture the `response.create`
/// frame, send the given event frames, then close.
async fn spawn_ws_server(events: Vec<String>, frame_capture: Arc<Mutex<Option<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.expect("ws handshake");
        if let Some(Ok(message)) = ws.next().await
            && let Ok(text) = message.into_text()
        {
            *frame_capture.lock().unwrap() = Some(text.to_string());
        }
        for event in &events {
            let _ = ws.send(Message::text(event.clone())).await;
        }
        let _ = ws.close(None).await;
    });
    format!("http://{addr}")
}

/// Combined server for the fallback test: fails the WebSocket upgrade (returns
/// HTTP 400) on the first connection, then serves SSE on the POST that follows.
async fn spawn_ws_fail_then_sse_server(
    sse_events: Vec<String>,
    capture: Arc<Mutex<Option<CapturedHttp>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let captured = read_http_request(&mut stream).await;
            let is_ws_upgrade = captured.method.eq_ignore_ascii_case("GET")
                && captured
                    .headers
                    .get("upgrade")
                    .map(|v| v.to_ascii_lowercase().contains("websocket"))
                    .unwrap_or(false);
            if is_ws_upgrade {
                // Reject the WS handshake -> client falls back to SSE.
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                let _ = stream.flush().await;
                continue;
            }
            *capture.lock().unwrap() = Some(captured);
            write_sse_ok(&mut stream, &sse_events).await;
            break;
        }
    });
    format!("http://{addr}")
}

// ============================================================================
// Shared fixtures
// ============================================================================

fn text_response_events() -> Vec<String> {
    vec![
        r#"{"type":"response.created","response":{"id":"resp_1"}}"#.to_string(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant"}}"#.to_string(),
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hello"}"#.to_string(),
        r#"{"type":"response.output_text.delta","output_index":0,"delta":" world"}"#.to_string(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Hello world"}]}}"#.to_string(),
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":2},"output_tokens":3,"total_tokens":15}}}"#.to_string(),
    ]
}

fn tool_call_events() -> Vec<String> {
    vec![
        r#"{"type":"response.created","response":{"id":"resp_2"}}"#.to_string(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"get_weather","arguments":""}}"#.to_string(),
        r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"city\":"}"#.to_string(),
        r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"paris\"}"}"#.to_string(),
        r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"city\":\"paris\"}"}"#.to_string(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"get_weather","arguments":"{\"city\":\"paris\"}"}}"#.to_string(),
        r#"{"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13}}}"#.to_string(),
    ]
}

fn user_request(base_url: &str, transport: Transport) -> (CodexStreamFn, StreamRequest) {
    let token = Arc::new(StaticTokenSource::new("test-bearer", "acct_test"));
    let stream_fn = CodexStreamFn::new(token)
        .with_base_url(base_url.to_string())
        .with_transport(transport)
        .with_ws_connect_timeout(Duration::from_secs(3));
    let context = LlmContext {
        system_prompt: "be terse".to_string(),
        messages: vec![ChatMessage::user("hi")],
        tools: vec![],
    };
    let request = StreamRequest::new(ModelSpec::from_name("gpt-5-codex"), context);
    (stream_fn, request)
}

async fn collect(
    stream: rust_genai_agent::AssistantMessageEventStream,
) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn terminal(events: &[AssistantMessageEvent]) -> &AssistantMessage {
    events
        .iter()
        .find_map(|e| e.terminal_message())
        .expect("a terminal event")
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn sse_request_construction_is_correct() {
    let capture = Arc::new(Mutex::new(None));
    let base_url = spawn_sse_server(text_response_events(), capture.clone()).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Sse);

    let stream = stream_fn.stream(request).await;
    let _ = collect(stream).await;

    let captured = capture.lock().unwrap().clone().expect("request captured");
    // URL path + method (resolveCodexUrl -> /codex/responses).
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/codex/responses");
    // Auth + account + Responses headers.
    assert_eq!(
        captured.headers.get("authorization").unwrap(),
        "Bearer test-bearer"
    );
    assert_eq!(
        captured.headers.get("chatgpt-account-id").unwrap(),
        "acct_test"
    );
    assert_eq!(
        captured.headers.get("openai-beta").unwrap(),
        "responses=experimental"
    );
    assert_eq!(captured.headers.get("accept").unwrap(), "text/event-stream");
    assert_eq!(
        captured.headers.get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(captured.headers.get("originator").unwrap(), "pi");
    // Body fields.
    let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
    assert_eq!(body["model"], "gpt-5-codex");
    assert_eq!(body["store"], serde_json::json!(false));
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["instructions"], "be terse");
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][0]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn sse_text_stream_maps_to_final_message() {
    let capture = Arc::new(Mutex::new(None));
    let base_url = spawn_sse_server(text_response_events(), capture).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Sse);

    let events = collect(stream_fn.stream(request).await).await;

    // Event protocol: exactly one terminal, and it is Done.
    assert!(matches!(
        events.first(),
        Some(AssistantMessageEvent::Start { .. })
    ));
    let terminal_count = events
        .iter()
        .filter(|e| e.terminal_message().is_some())
        .count();
    assert_eq!(terminal_count, 1);
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
    // At least one text delta was emitted.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. }))
    );

    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.text(), "Hello world");
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    // Usage: OpenAI-inclusive input (matches AgentUsage::from convention).
    assert_eq!(message.usage.input_tokens, 12);
    assert_eq!(message.usage.output_tokens, 3);
    assert_eq!(message.usage.total_tokens, 15);
    assert_eq!(message.usage.cache_read_tokens, 2);
}

#[tokio::test]
async fn sse_tool_call_stream_maps_to_tool_use() {
    let capture = Arc::new(Mutex::new(None));
    let base_url = spawn_sse_server(tool_call_events(), capture).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Sse);

    let events = collect(stream_fn.stream(request).await).await;
    let message = terminal(&events);

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let tool_calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "get_weather");
    assert_eq!(tool_calls[0].id, "call_1|fc_1");
    assert_eq!(
        tool_calls[0].arguments,
        serde_json::json!({ "city": "paris" })
    );
}

#[tokio::test]
async fn websocket_stream_maps_to_final_message() {
    let frame_capture = Arc::new(Mutex::new(None));
    let base_url = spawn_ws_server(text_response_events(), frame_capture.clone()).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Websocket);

    let events = collect(stream_fn.stream(request).await).await;

    // The client sent a `response.create` frame carrying the request body.
    let frame = frame_capture.lock().unwrap().clone().expect("create frame");
    let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["type"], "response.create");
    assert_eq!(frame["model"], "gpt-5-codex");
    assert_eq!(frame["stream"], serde_json::json!(true));
    assert_eq!(frame["input"][0]["content"][0]["text"], "hi");

    // Same event mapping as SSE.
    let message = terminal(&events);
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.text(), "Hello world");
    assert_eq!(message.usage.total_tokens, 15);
}

#[tokio::test]
async fn websocket_upgrade_failure_falls_back_to_sse() {
    let capture = Arc::new(Mutex::new(None));
    // Auto = WebSocket with SSE fallback; the server rejects the WS upgrade.
    let base_url = spawn_ws_fail_then_sse_server(text_response_events(), capture.clone()).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Auto);

    let events = collect(stream_fn.stream(request).await).await;

    // The SSE POST was reached (fallback happened) and produced the final message.
    let captured = capture
        .lock()
        .unwrap()
        .clone()
        .expect("sse fallback request");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/codex/responses");

    let message = terminal(&events);
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
    assert_eq!(message.text(), "Hello world");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn non_2xx_handshake_is_in_band_terminal_error() {
    let body = r#"{"error":{"code":"usage_limit_reached","plan_type":"Plus"}}"#.to_string();
    let base_url = spawn_status_server("429 Too Many Requests", body).await;
    let (stream_fn, request) = user_request(&base_url, Transport::Sse);

    let events = collect(stream_fn.stream(request).await).await;

    // Terminal is an in-band Error (never a panic / thrown error), reason Error.
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
    let message = terminal(&events);
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.as_deref().unwrap_or_default();
    assert!(
        error.starts_with("You have hit your ChatGPT usage limit"),
        "unexpected error message: {error}"
    );
    // No Done event was emitted.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::Done { .. }))
    );
}

#[tokio::test]
async fn cancellation_mid_stream_yields_aborted_terminal() {
    let prefix = vec![
        r#"{"type":"response.created","response":{"id":"resp_c"}}"#.to_string(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant"}}"#.to_string(),
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"partial"}"#.to_string(),
    ];
    let base_url = spawn_stalling_sse_server(prefix).await;

    let token = Arc::new(StaticTokenSource::new("test-bearer", "acct_test"));
    let stream_fn = CodexStreamFn::new(token)
        .with_base_url(base_url)
        .with_transport(Transport::Sse);
    let cancel = CancellationToken::new();
    let context = LlmContext {
        system_prompt: String::new(),
        messages: vec![ChatMessage::user("hi")],
        tools: vec![],
    };
    let request = StreamRequest::new(ModelSpec::from_name("gpt-5-codex"), context)
        .with_cancellation(cancel.clone());

    let mut stream = stream_fn.stream(request).await;
    let mut saw_delta = false;
    let mut terminal_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        if let AssistantMessageEvent::TextDelta { .. } = event {
            saw_delta = true;
            // Cancel mid-stream, after content has started flowing.
            cancel.cancel();
        }
        if let Some(message) = event.terminal_message() {
            terminal_message = Some(message.clone());
            break;
        }
    }

    assert!(saw_delta, "expected a text delta before cancelling");
    let message = terminal_message.expect("a terminal event after cancel");
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request aborted by user")
    );
    // Partial content is retained on the aborted message.
    assert_eq!(message.text(), "partial");
    // Sanity: this content matches AssistantContent::text semantics.
    assert!(
        message
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::Text { text, .. } if text == "partial"))
    );
}
