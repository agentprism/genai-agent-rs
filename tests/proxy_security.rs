#![cfg(feature = "proxy")]

use futures::StreamExt;
use genai::chat::ChatMessage;
use rust_genai_agent::proxy::{ProxyConfigError, ProxyStreamOptions, stream_proxy};
use rust_genai_agent::{
    AssistantContent, AssistantMessageEvent, LlmContext, StopReason, StreamRequest,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// Generous deadlock safety-net (not a latency assertion): a tight bound only fires spuriously
// under CPU-saturated runs. The 500ms accept-poll further down IS an intentional short window.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn default_request() -> StreamRequest {
    StreamRequest::new(
        "gpt-4o",
        LlmContext {
            system_prompt: "security regression".to_owned(),
            messages: vec![ChatMessage::user("hello")],
            tools: Vec::new(),
        },
    )
}

async fn read_request(socket: &mut TcpStream) -> Vec<u8> {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    let mut request = Vec::new();
    let mut scratch = [0_u8; 4096];
    let header_end = loop {
        let read = socket
            .read(&mut scratch)
            .await
            .expect("read test HTTP request");
        assert_ne!(read, 0, "request ended before its headers");
        request.extend_from_slice(&scratch[..read]);
        assert!(
            request.len() <= MAX_REQUEST_BYTES,
            "test request exceeded fixture bound"
        );
        if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let head = String::from_utf8_lossy(&request[..header_end]);
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or_default();
    while request.len() < header_end + content_length {
        let read = socket
            .read(&mut scratch)
            .await
            .expect("read test HTTP request body");
        assert_ne!(read, 0, "request ended before its declared body");
        request.extend_from_slice(&scratch[..read]);
        assert!(
            request.len() <= MAX_REQUEST_BYTES,
            "test request exceeded fixture bound"
        );
    }
    request
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

fn sse_response(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    let mut body = Vec::new();
    for event in events {
        body.extend_from_slice(b"data: ");
        body.extend_from_slice(
            serde_json::to_string(&event)
                .expect("serialize test proxy event")
                .as_bytes(),
        );
        body.extend_from_slice(b"\r\n\r\n");
    }
    http_response("200 OK", "text/event-stream", body)
}

fn successful_sse_response() -> Vec<u8> {
    sse_response([
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
    ])
}

async fn spawn_response_server(response: Vec<u8>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind response server");
    let address = listener.local_addr().expect("response server address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept proxy request");
        let _request = read_request(&mut socket).await;
        socket
            .write_all(&response)
            .await
            .expect("write test HTTP response");
    });
    (format!("http://{address}"), task)
}

async fn collect(options: ProxyStreamOptions) -> Vec<AssistantMessageEvent> {
    let stream = stream_proxy(default_request(), options).await;
    tokio::time::timeout(TEST_TIMEOUT, stream.collect::<Vec<_>>())
        .await
        .expect("proxy stream must settle within the test timeout")
}

#[test]
fn proxy_urls_with_any_userinfo_are_rejected_before_normalization() {
    let cases = [
        "https://alice@proxy.example.test/base",
        "https://alice:visible-password@proxy.example.test/base",
        "https://:visible-password@proxy.example.test/base",
        "https://%61lice@proxy.example.test/base",
        "https://%61lice:%76isible-password@proxy.example.test/base",
    ];

    for url in cases {
        let error = ProxyStreamOptions::new(url, "bearer-only-token")
            .expect_err("proxy URL userinfo must be rejected");
        assert!(matches!(&error, ProxyConfigError::UserInfoNotAllowed));
        let debug = format!("{error:?}");
        assert!(!debug.contains("alice"));
        assert!(!debug.contains("visible-password"));
        assert!(!debug.contains("bearer-only-token"));
    }
}

#[tokio::test]
async fn whitespace_only_sse_data_is_ignored_between_valid_events() {
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata:    \t  \r\n\r\n".to_vec();
    let valid = successful_sse_response();
    let valid_body = valid
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|position| position + 4)
        .expect("fixture has HTTP headers");
    response.extend_from_slice(&valid[valid_body..]);

    let (base_url, server) = spawn_response_server(response).await;
    let options = ProxyStreamOptions::new(base_url, "test-token").expect("valid proxy options");
    let events = collect(options).await;
    server.await.expect("response server task");

    assert_eq!(events.len(), 2, "empty data must not become an error event");
    assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
    assert!(matches!(
        events[1],
        AssistantMessageEvent::Done {
            reason: StopReason::Stop,
            ..
        }
    ));
}

#[tokio::test]
async fn default_client_does_not_follow_redirects_or_forward_bearer_auth() {
    let target_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect target");
    let target_address = target_listener
        .local_addr()
        .expect("redirect target address");
    let (redirect_sent, redirect_received) = oneshot::channel();
    let target_task = tokio::spawn(async move {
        redirect_received
            .await
            .expect("redirect source must send its response");
        match tokio::time::timeout(Duration::from_millis(500), target_listener.accept()).await {
            Ok(Ok((mut socket, _))) => {
                let request = read_request(&mut socket).await;
                let has_authorization = String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains("\r\nauthorization:");
                socket
                    .write_all(&successful_sse_response())
                    .await
                    .expect("write redirect-target response");
                (true, has_authorization)
            }
            Ok(Err(error)) => panic!("redirect target accept failed: {error}"),
            Err(_) => (false, false),
        }
    });

    let redirect_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect source");
    let redirect_address = redirect_listener
        .local_addr()
        .expect("redirect source address");
    let location = format!("http://{target_address}/api/stream");
    let redirect_task = tokio::spawn(async move {
        let (mut socket, _) = redirect_listener
            .accept()
            .await
            .expect("accept redirect-source request");
        let _request = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write redirect response");
        redirect_sent
            .send(())
            .expect("redirect target task must remain active");
    });

    let options = ProxyStreamOptions::new(
        format!("http://{redirect_address}"),
        "redirect-secret-canary",
    )
    .expect("valid proxy options");
    let events = collect(options).await;
    redirect_task.await.expect("redirect source task");
    let (target_was_contacted, target_received_auth) =
        target_task.await.expect("redirect target task");

    assert!(
        !target_was_contacted,
        "the default proxy client followed an untrusted redirect"
    );
    assert!(
        !target_received_auth,
        "the default proxy client forwarded bearer auth to a redirect target"
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error { .. })
    ));
}

#[tokio::test]
async fn proxy_tool_argument_raw_limit_is_inclusive_and_preserves_partial_call() {
    let maximum_sized_delta = " ".repeat(1024 * 1024);
    let response = sse_response([
        json!({"type": "start"}),
        json!({
            "type": "toolcall_start",
            "contentIndex": 0,
            "id": "call-security",
            "toolName": "bounded_tool"
        }),
        json!({
            "type": "toolcall_delta",
            "contentIndex": 0,
            "delta": maximum_sized_delta
        }),
        json!({
            "type": "toolcall_delta",
            "contentIndex": 0,
            "delta": "x"
        }),
        json!({
            "type": "toolcall_end",
            "contentIndex": 0,
            "thoughtSignatures": []
        }),
        json!({
            "type": "done",
            "reason": "toolUse",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0
            }
        }),
    ]);
    let (base_url, server) = spawn_response_server(response).await;
    let options = ProxyStreamOptions::new(base_url, "test-token").expect("valid proxy options");
    let events = collect(options).await;
    server.await.expect("response server task");

    let error = match events.last() {
        Some(AssistantMessageEvent::Error { error, .. }) => error,
        other => panic!("expected one in-band terminal Error, got {other:?}"),
    };
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::Error { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::ToolCallDelta { delta, .. } if delta.len() == 1024 * 1024
    )));
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("tool-call argument"))
    );
    let call = match error.content.as_slice() {
        [AssistantContent::ToolCall(call)] => call,
        other => panic!("partial tool call was not preserved: {other:?}"),
    };
    assert_eq!(call.id, "call-security");
    assert_eq!(call.name, "bounded_tool");
    assert_eq!(call.arguments, json!({}));
}
