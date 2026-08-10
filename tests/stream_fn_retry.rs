//! Retry-layer coverage for `GenaiStreamFn::with_retry`, exercised against a local scripted TCP
//! server (the genai fork's own offline test pattern — no network, no keys).
//!
//! The retry layer peeks the first stream event: a retryable HTTP handshake error re-issues the
//! whole request after a pi-mirroring delay, while real content, non-retryable errors, and
//! cap-exceeded server delays are surfaced without retrying. These tests pin each of those paths,
//! plus cancellation during the retry sleep, using very small delays.

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ModelSpec, ServiceTarget};
use rust_genai_agent::{
    Agent, AgentConfig, AgentState, AssistantMessage, CancellationToken, GenaiStreamFn, LlmContext,
    RetryPolicy, StopReason, StreamFn, StreamRequest,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

// Generous deadlock safety-net (not a latency assertion): a tight bound only fires spuriously under
// CPU-saturated runs. The retry-delay *lower bounds* asserted below (`elapsed() >= 15ms/300ms`) are
// the real timing assertions and are unaffected by this ceiling.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a one-shot-per-connection scripted HTTP server: the Nth accepted connection is answered
/// with `responses[N]`. Returns the base URL and a counter of accepted connections (i.e., the
/// number of handshake attempts the retry layer actually made).
async fn spawn_scripted_server(responses: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local scripted server");
    let address = listener.local_addr().expect("scripted server address");
    let connections = Arc::new(AtomicUsize::new(0));
    let connections_bg = connections.clone();
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            connections_bg.fetch_add(1, Ordering::SeqCst);
            // Best-effort: read the request head before answering.
            let mut buffer = [0_u8; 8192];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{address}/"), connections)
}

/// A non-2xx HTTP response with the given status line and pre-formatted extra header block (each
/// line already `\r\n`-terminated, or empty).
fn http_error(status_line: &str, extra_headers: &str) -> String {
    let body = r#"{"error":{"message":"rate limited"}}"#;
    format!(
        "HTTP/1.1 {status_line}\r\n\
         content-type: application/json\r\n\
         {extra_headers}\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// A 200 SSE response that streams `Hello` and terminates cleanly with `[DONE]`.
fn http_sse_hello() -> String {
    let chunk = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    format!(
        "HTTP/1.1 200 OK\r\n\
         content-type: text/event-stream\r\n\
         connection: close\r\n\
         \r\n\
         data: {chunk}\n\ndata: [DONE]\n\n"
    )
}

/// A 200 SSE response that streams `Hello` and then closes without `[DONE]` — content is emitted,
/// then the stream fails mid-flight (a `finish_without_end` terminal error).
fn http_sse_hello_no_done() -> String {
    let chunk = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    format!(
        "HTTP/1.1 200 OK\r\n\
         content-type: text/event-stream\r\n\
         connection: close\r\n\
         \r\n\
         data: {chunk}\n\n"
    )
}

fn local_target(url: String) -> ModelSpec {
    ModelSpec::from_target(ServiceTarget {
        endpoint: Endpoint::from_owned(url),
        auth: AuthData::from_single("test-key"),
        model: ModelIden::new(AdapterKind::OpenAI, "gpt-test"),
    })
}

fn request(url: &str) -> StreamRequest {
    StreamRequest::new(
        local_target(url.to_string()),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    )
}

/// Drive a stream function to its terminal assistant message.
async fn drive(stream_fn: &GenaiStreamFn, url: &str) -> AssistantMessage {
    drive_request(stream_fn, request(url)).await
}

/// Drive a stream function with an explicit request to its terminal assistant message.
async fn drive_request(stream_fn: &GenaiStreamFn, request: StreamRequest) -> AssistantMessage {
    let stream = timeout(TEST_TIMEOUT, stream_fn.stream(request))
        .await
        .expect("stream() returned before timeout");
    let result = stream.result_handle();
    let _events: Vec<_> = timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message")
}

#[tokio::test]
async fn retries_429_with_retry_after_ms_then_succeeds() {
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 20\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let start = Instant::now();
    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.text(), "Hello");
    assert!(message.error_message.is_none(), "success carries no error");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "one retry then success = two handshakes"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(15),
        "the retry honored the ~20ms server delay"
    );
}

#[tokio::test]
async fn retries_retryable_503() {
    let (url, connections) = spawn_scripted_server(vec![
        http_error("503 Service Unavailable", "retry-after-ms: 20\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.text(), "Hello");
    assert!(message.error_message.is_none());
    assert_eq!(connections.load(Ordering::SeqCst), 2, "503 is retryable");
}

#[tokio::test]
async fn does_not_retry_non_retryable_400() {
    // Script the error twice: a wrongful retry would be caught by `connections == 2`.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("400 Bad Request", ""),
        http_error("400 Bad Request", ""),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message.error_message.as_deref().unwrap().contains("400"),
        "surfaces the 400 error: {:?}",
        message.error_message
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1, "400 is not retried");
}

#[tokio::test]
async fn honors_x_should_retry_false_on_429() {
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "x-should-retry: false\r\n"),
        http_error("429 Too Many Requests", "x-should-retry: false\r\n"),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "x-should-retry:false is a hard no-retry override even on a 429"
    );
}

#[tokio::test]
async fn server_delay_over_cap_fails_fast_with_pi_exact_message() {
    // retry-after-ms: 1500 (1.5s) with a 1000ms (1s) cap: ceil(1.5)=2, ceil(1.0)=1.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 1500\r\n"),
        http_error("429 Too Many Requests", "retry-after-ms: 1500\r\n"),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 1000,
    });

    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.as_deref().unwrap();
    assert!(
        error_message.starts_with("Server requested 2s retry delay (max: 1s). "),
        "byte-exact pi cap message prefix, was: {error_message:?}"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "a cap-exceeding server delay fails fast without retrying"
    );
}

#[tokio::test]
async fn exponential_backoff_is_not_subject_to_the_cap() {
    // No retry-after headers -> exponential backoff (~500ms for the first retry). Even with a 1ms
    // cap (which would fail-fast any *server-requested* delay), the computed backoff still retries.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("503 Service Unavailable", ""),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 2,
        max_retry_delay_ms: 1,
    });

    let start = Instant::now();
    let message = drive(&stream_fn, &url).await;

    assert_eq!(message.text(), "Hello");
    assert!(message.error_message.is_none());
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "exponential backoff retried despite the tiny cap"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "the ~500ms exponential backoff actually elapsed"
    );
}

#[tokio::test]
async fn retries_zero_is_byte_identical_to_no_retry() {
    // Default construction (no `with_retry`) and an explicit `max_retries: 0` policy must behave
    // identically on a would-be-retryable 429: one handshake, same terminal error text.
    let (url_default, connections_default) = spawn_scripted_server(vec![http_error(
        "429 Too Many Requests",
        "retry-after-ms: 20\r\n",
    )])
    .await;
    let (url_zero, connections_zero) = spawn_scripted_server(vec![http_error(
        "429 Too Many Requests",
        "retry-after-ms: 20\r\n",
    )])
    .await;

    let default_fn = GenaiStreamFn::new(Client::builder().build());
    let zero_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 0,
        max_retry_delay_ms: 60_000,
    });

    let default_message = drive(&default_fn, &url_default).await;
    let zero_message = drive(&zero_fn, &url_zero).await;

    assert_eq!(default_message.stop_reason, StopReason::Error);
    assert_eq!(zero_message.stop_reason, StopReason::Error);
    assert_eq!(
        default_message.error_message, zero_message.error_message,
        "max_retries=0 is byte-identical to the no-retry path"
    );
    assert_eq!(connections_default.load(Ordering::SeqCst), 1);
    assert_eq!(connections_zero.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_retry_sleep_aborts_promptly() {
    // A large server-requested delay with the cap disabled would sleep 5s; cancelling mid-sleep
    // must abort in-band well before that, and must not issue the retry.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 5000\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 0,
    });

    let cancel = CancellationToken::new();
    let request = request(&url).with_cancellation(cancel.clone());
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });

    let stream = timeout(Duration::from_secs(30), stream_fn.stream(request))
        .await
        .expect("stream() aborted promptly, not after the 5s server delay");
    let result = stream.result_handle();
    let _events: Vec<_> = stream.collect().await;
    let message = result.get().await.expect("terminal message");

    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request aborted by user")
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the retry was never issued after cancellation"
    );
    canceller.await.unwrap();
}

#[tokio::test]
async fn out_of_range_server_delay_saturates_and_cancellation_wins_over_sleeping() {
    // `retry-after-ms: 1e30` exceeds `Duration`'s range: with the cap disabled (a per-request
    // `max_retry_delay_ms(0)` override of the construction policy), the delay must safely
    // saturate instead of panicking (the never-throw contract), and cancelling during the wait
    // must still abort promptly without issuing the retry.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 1e30\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let cancel = CancellationToken::new();
    let request = request(&url)
        .with_cancellation(cancel.clone())
        .with_max_retry_delay_ms(0);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });

    let stream = timeout(Duration::from_secs(30), stream_fn.stream(request))
        .await
        .expect("stream() aborted promptly, not after the saturated delay");
    let result = stream.result_handle();
    let _events: Vec<_> = stream.collect().await;
    let message = result.get().await.expect("terminal message");

    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request aborted by user")
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the retry was never issued after cancellation"
    );
    canceller.await.unwrap();
}

#[tokio::test]
async fn content_then_mid_stream_error_is_not_retried() {
    // The handshake succeeds and content is emitted, then the stream fails without an End event.
    // Because a content event was already observed, the mid-stream failure is never retried.
    let (url, connections) =
        spawn_scripted_server(vec![http_sse_hello_no_done(), http_sse_hello()]).await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive(&stream_fn, &url).await;

    assert_eq!(
        message.text(),
        "Hello",
        "content emitted before the failure is retained"
    );
    assert_eq!(
        message.stop_reason,
        StopReason::Error,
        "the mid-stream close surfaces as a terminal error"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "content already emitted -> never retried"
    );
}

// ---------------------------------------------------------------------------
// CORE-REQ-01: per-request retry fields override the construction-time policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_request_max_retries_enables_retries_when_policy_disables_them() {
    // Default construction performs no retries; the request's own `max_retries` turns them on.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 20\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build());

    let message = drive_request(&stream_fn, request(&url).with_max_retries(3)).await;

    assert_eq!(message.text(), "Hello");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "the per-request max_retries drove one retry"
    );
}

#[tokio::test]
async fn per_request_max_retries_zero_disables_the_construction_policy() {
    // The construction policy would retry; a per-request `max_retries: 0` opts the request out.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 20\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive_request(&stream_fn, request(&url).with_max_retries(0)).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the per-request opt-out suppressed the construction policy's retry"
    );
}

#[tokio::test]
async fn per_request_max_retry_delay_ms_overrides_the_policy_cap() {
    // retry-after-ms: 5000 (5s) fits the construction policy's 60s cap but exceeds the request's
    // 1s cap: ceil(5.0)=5, ceil(1.0)=1.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 5000\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build()).with_retry(RetryPolicy {
        max_retries: 3,
        max_retry_delay_ms: 60_000,
    });

    let message = drive_request(&stream_fn, request(&url).with_max_retry_delay_ms(1_000)).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    let error_message = message.error_message.as_deref().unwrap();
    assert!(
        error_message.starts_with("Server requested 5s retry delay (max: 1s). "),
        "byte-exact pi cap message prefix with the per-request cap, was: {error_message:?}"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the per-request cap fails fast without retrying"
    );
}

#[tokio::test]
async fn agent_max_retries_drive_real_retries_end_to_end() {
    // The facade forwards its configured retry fields onto every stream request; a GenaiStreamFn
    // with the default (disabled) construction policy therefore still retries.
    let (url, connections) = spawn_scripted_server(vec![
        http_error("429 Too Many Requests", "retry-after-ms: 20\r\n"),
        http_sse_hello(),
    ])
    .await;
    let stream_fn = GenaiStreamFn::new(Client::builder().build());
    let state = AgentState {
        model: local_target(url),
        ..AgentState::default()
    };
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(stream_fn))
            .with_max_retries(2),
    );

    agent
        .prompt("Why is the sky red?")
        .await
        .expect("admission succeeds");

    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "AgentConfig.max_retries reached GenaiStreamFn through the stream request"
    );
    let state = agent.state();
    let last = state.messages.last().expect("assistant message");
    assert_eq!(last.role(), "assistant");
    match last {
        rust_genai_agent::AgentMessage::Assistant(message) => {
            assert_eq!(message.stop_reason, StopReason::Stop);
            assert_eq!(message.text(), "Hello");
        }
        other => panic!("expected an assistant message, got {other:?}"),
    }
}
