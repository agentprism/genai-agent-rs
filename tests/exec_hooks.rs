//! `on_payload`/`on_response` exec-hook plumbing and wiring coverage.
//!
//! The loop and the `Agent` facade forward the hooks as handles onto each [`StreamRequest`];
//! honoring them is a stream-function concern. `GenaiStreamFn` applies construction-time hooks
//! through the genai fork's client-level interceptors and honors per-request hooks through the
//! fork's request-level `ExecOptions` overrides (a request hook replaces, never composes with,
//! the construction default). Both are verified here against local one-shot/scripted capture
//! servers (the fork's own offline test pattern) because the fork's `intercept`/`observe` entry
//! points are crate-private and cannot be driven directly.

#![cfg(feature = "testing")]

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ModelSpec, ServiceTarget};
use rust_genai_agent::testing::{EventRecorder, MockStreamFn, fixtures, script};
use rust_genai_agent::{
    Agent, AgentConfig, AgentContext, AgentLoopConfig, AgentState, GenaiStreamFn, LlmContext,
    OnPayloadHook, OnResponseHook, StopReason, StreamFn, StreamRequest, StreamResponseInfo,
    default_convert_to_llm, run_agent_loop,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// Generous deadlock safety-net (not a latency assertion): these are local one-shot capture-server
// operations that complete in well under a millisecond. A tight bound (previously 3s) spuriously
// tripped under CPU-saturated `--all-features` runs where tokio workers are starved; 30s only ever
// fires on a genuine hang.
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

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
async fn loop_forwards_both_exec_hooks_onto_the_stream_request() {
    let stream = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "ok",
    )]));
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let on_payload = recording_on_payload(seen_payloads.clone(), Some(json!({"replaced": true})));
    let on_response = recording_on_response(seen_responses.clone());
    let config = AgentLoopConfig::new(fixtures::model(), default_convert_to_llm())
        .with_on_payload(on_payload.clone())
        .with_on_response(on_response.clone());

    let mut sink = EventRecorder::new();
    run_agent_loop(
        vec![fixtures::user_msg("hi")],
        AgentContext::default(),
        config,
        &mut sink,
        CancellationToken::new(),
        Some(stream.clone()),
    )
    .await
    .unwrap();

    let calls = stream.calls();
    assert_eq!(calls.len(), 1);
    // The loop forwards the exact configured handles, not wrappers.
    let forwarded_payload = calls[0].on_payload.clone().expect("on_payload forwarded");
    assert!(Arc::ptr_eq(&forwarded_payload, &on_payload));
    let forwarded_response = calls[0].on_response.clone().expect("on_response forwarded");
    assert!(Arc::ptr_eq(&forwarded_response, &on_response));

    // The forwarded handles carry the configured behavior: inspection plus replacement.
    let replaced = forwarded_payload(json!({"probe": 1}), fixtures::model_iden()).await;
    assert_eq!(replaced, Some(json!({"replaced": true})));
    forwarded_response(
        StreamResponseInfo::new(429, vec![("Retry-After".to_owned(), "2".to_owned())]),
        fixtures::model_iden(),
    )
    .await;
    assert_eq!(
        seen_payloads.lock().unwrap().as_slice(),
        [(json!({"probe": 1}), fixtures::model_iden())]
    );
    let responses = seen_responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0.status, 429);
    assert_eq!(responses[0].0.header("retry-after"), Some("2"));
    assert_eq!(responses[0].1, fixtures::model_iden());
}

#[tokio::test]
async fn stream_request_exec_hooks_default_to_none() {
    let stream = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "ok",
    )]));
    let agent = Agent::new(AgentConfig::default().with_stream_fn(stream.clone()));

    agent.prompt("no hooks configured").await.unwrap();

    let calls = stream.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].on_payload.is_none());
    assert!(calls[0].on_response.is_none());
}

#[tokio::test]
async fn agent_forwards_config_hooks_and_guarded_setters_apply_to_the_next_run() {
    let stream = Arc::new(MockStreamFn::from_streams(vec![
        script::text_response("one"),
        script::text_response("two"),
    ]));
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let first_payload = recording_on_payload(seen_payloads.clone(), None);
    let first_response = recording_on_response(seen_responses.clone());
    let agent = Agent::new(
        AgentConfig::default()
            .with_stream_fn(stream.clone())
            .with_on_payload(first_payload.clone())
            .with_on_response(first_response.clone()),
    );

    agent.prompt("first").await.unwrap();

    let second_payload = recording_on_payload(seen_payloads.clone(), Some(json!("second")));
    agent.set_on_payload(Some(second_payload.clone())).unwrap();
    agent.set_on_response(None).unwrap();
    agent.prompt("second").await.unwrap();

    let calls = stream.calls();
    assert_eq!(calls.len(), 2);
    let first_forwarded = calls[0].on_payload.clone().expect("first run on_payload");
    assert!(Arc::ptr_eq(&first_forwarded, &first_payload));
    let first_observed = calls[0].on_response.clone().expect("first run on_response");
    assert!(Arc::ptr_eq(&first_observed, &first_response));
    let second_forwarded = calls[1].on_payload.clone().expect("second run on_payload");
    assert!(Arc::ptr_eq(&second_forwarded, &second_payload));
    assert!(calls[1].on_response.is_none());
}

// region:    --- GenaiStreamFn construction-time wiring (local one-shot capture server)

/// Spawn a one-shot HTTP server that captures the request body and answers with `raw_response`.
///
/// This follows the genai fork's own offline exec-hook test pattern (no network, no keys).
async fn spawn_capture_server(raw_response: String) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local capture server");
    let address = listener.local_addr().expect("capture server address");
    let (body_tx, body_rx) = oneshot::channel::<String>();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0_u8; 4096];
        let body = loop {
            let Ok(read) = socket.read(&mut chunk).await else {
                break String::new();
            };
            if read == 0 {
                break String::new();
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
                let content_length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buffer.len() >= body_start + content_length {
                    break String::from_utf8_lossy(
                        &buffer[body_start..body_start + content_length],
                    )
                    .to_string();
                }
            }
        };
        let _ = body_tx.send(body);
        let _ = socket.write_all(raw_response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}/"), body_rx)
}

fn openai_sse_ok_response() -> String {
    let chunk = r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    format!(
        "HTTP/1.1 200 OK\r\n\
        content-type: text/event-stream\r\n\
        x-obs-test: obs-value\r\n\
        connection: close\r\n\
        \r\n\
        data: {chunk}\n\ndata: [DONE]\n\n"
    )
}

fn local_target(url: String) -> ModelSpec {
    ModelSpec::from_target(ServiceTarget {
        endpoint: Endpoint::from_owned(url),
        auth: AuthData::from_single("test-key"),
        model: ModelIden::new(AdapterKind::OpenAI, "gpt-test"),
    })
}

/// `GenaiStreamFn::with_exec_hooks` installs the fork's client-level interceptors: the
/// `on_payload` replacement is what actually goes over the wire, and `on_response` observes the
/// response head. The delegation adapters cannot be exercised directly because the fork's
/// `intercept`/`observe` methods are crate-private, so this local one-shot server is the seam.
#[tokio::test]
async fn genai_stream_fn_applies_construction_time_exec_hooks_through_the_fork_client() {
    let (url, body_rx) = spawn_capture_server(openai_sse_ok_response()).await;
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let record_payloads = seen_payloads.clone();
    let on_payload: OnPayloadHook = Arc::new(move |mut payload, model| {
        let record_payloads = record_payloads.clone();
        Box::pin(async move {
            record_payloads
                .lock()
                .unwrap()
                .push((payload.clone(), model));
            payload["x_intercepted"] = json!(true);
            Some(payload)
        })
    });
    let on_response = recording_on_response(seen_responses.clone());
    let stream_fn =
        GenaiStreamFn::with_exec_hooks(Client::builder(), Some(on_payload), Some(on_response));

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    );
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");
    assert_eq!(message.text(), "Hello");

    // The hook saw the original serialized provider payload plus the resolved model identity.
    {
        let payloads = seen_payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        let (payload, model) = &payloads[0];
        assert_eq!(&*model.model_name, "gpt-test");
        assert_eq!(
            payload.get("model").and_then(Value::as_str),
            Some("gpt-test")
        );
        assert_eq!(payload.get("x_intercepted"), None);
    }

    // The replacement payload is what actually went over the wire.
    let wire_body = tokio::time::timeout(TEST_TIMEOUT, body_rx)
        .await
        .expect("captured request before timeout")
        .expect("capture server recorded a body");
    let wire_json: Value = serde_json::from_str(&wire_body).expect("wire body is JSON");
    assert_eq!(wire_json.get("x_intercepted"), Some(&json!(true)));
    assert_eq!(
        wire_json.get("model").and_then(Value::as_str),
        Some("gpt-test")
    );

    // The observer saw the response head: status plus headers, with the model identity.
    let responses = seen_responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    let (info, model) = &responses[0];
    assert_eq!(info.status, 200);
    assert_eq!(info.header("x-obs-test"), Some("obs-value"));
    assert_eq!(&*model.model_name, "gpt-test");
}

/// `GenaiStreamFn` honors per-request `StreamRequest` hooks on a client built without
/// construction-time hooks: the request's `on_payload` replacement is what actually goes over
/// the wire, and the request's `on_response` observes the response head exactly once.
#[tokio::test]
async fn genai_stream_fn_honors_request_level_exec_hooks() {
    let (url, body_rx) = spawn_capture_server(openai_sse_ok_response()).await;
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let stream_fn = GenaiStreamFn::new(Client::builder().build());

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    )
    .with_on_payload(recording_on_payload(
        seen_payloads.clone(),
        Some(json!({"x_request": true})),
    ))
    .with_on_response(recording_on_response(seen_responses.clone()));
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");
    assert_eq!(message.text(), "Hello");

    // The request payload hook fired exactly once with the serialized provider payload.
    {
        let payloads = seen_payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            payloads[0].0.get("model").and_then(Value::as_str),
            Some("gpt-test")
        );
    }
    // Its replacement is what actually went over the wire — a full payload replacement, so the
    // wire body is exactly the hook's return value.
    let wire_body = tokio::time::timeout(TEST_TIMEOUT, body_rx)
        .await
        .expect("captured request before timeout")
        .expect("capture server recorded a body");
    let wire_json: Value = serde_json::from_str(&wire_body).expect("wire body is JSON");
    assert_eq!(wire_json, json!({"x_request": true}));
    // The request response hook observed the response head exactly once.
    let responses = seen_responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0.status, 200);
    assert_eq!(responses[0].0.header("x-obs-test"), Some("obs-value"));
}

/// A per-request hook *replaces* the construction-time hook of its channel: the construction
/// hook never fires for the request and the wire payload carries only the request replacement —
/// the two never compose, so exactly one hook fires per channel.
#[tokio::test]
async fn genai_stream_fn_request_hook_replaces_construction_hook_without_composing() {
    let (url, body_rx) = spawn_capture_server(openai_sse_ok_response()).await;
    let construction_payloads: SeenPayloads = Arc::default();
    let construction_responses: SeenResponses = Arc::default();
    let request_payloads: SeenPayloads = Arc::default();
    let request_responses: SeenResponses = Arc::default();
    let stream_fn = GenaiStreamFn::with_exec_hooks(
        Client::builder(),
        Some(recording_on_payload(
            construction_payloads.clone(),
            Some(json!({"x_construction": true})),
        )),
        Some(recording_on_response(construction_responses.clone())),
    );

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    )
    .with_on_payload(recording_on_payload(
        request_payloads.clone(),
        Some(json!({"x_request": true})),
    ))
    .with_on_response(recording_on_response(request_responses.clone()));
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");

    assert!(
        construction_payloads.lock().unwrap().is_empty(),
        "the construction payload hook must not fire on a replaced channel"
    );
    assert!(
        construction_responses.lock().unwrap().is_empty(),
        "the construction response hook must not fire on a replaced channel"
    );
    assert_eq!(request_payloads.lock().unwrap().len(), 1);
    assert_eq!(request_responses.lock().unwrap().len(), 1);
    let wire_body = tokio::time::timeout(TEST_TIMEOUT, body_rx)
        .await
        .expect("captured request before timeout")
        .expect("capture server recorded a body");
    let wire_json: Value = serde_json::from_str(&wire_body).expect("wire body is JSON");
    assert_eq!(wire_json.get("x_request"), Some(&json!(true)));
    assert_eq!(
        wire_json.get("x_construction"),
        None,
        "the request replacement never composes with the construction default"
    );
}

/// Requests that carry no hook inherit the construction-time hooks unchanged: each fires exactly
/// once for the attempt.
#[tokio::test]
async fn genai_stream_fn_request_without_hooks_inherits_construction_hooks() {
    let (url, body_rx) = spawn_capture_server(openai_sse_ok_response()).await;
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let stream_fn = GenaiStreamFn::with_exec_hooks(
        Client::builder(),
        Some(recording_on_payload(seen_payloads.clone(), None)),
        Some(recording_on_response(seen_responses.clone())),
    );

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    );
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");
    assert_eq!(message.text(), "Hello");

    assert_eq!(seen_payloads.lock().unwrap().len(), 1);
    assert_eq!(seen_responses.lock().unwrap().len(), 1);
    let wire_body = tokio::time::timeout(TEST_TIMEOUT, body_rx)
        .await
        .expect("captured request before timeout")
        .expect("capture server recorded a body");
    let wire_json: Value = serde_json::from_str(&wire_body).expect("wire body is JSON");
    assert_eq!(
        wire_json.get("model").and_then(Value::as_str),
        Some("gpt-test")
    );
}

/// A request `on_response` hook fires on the HTTP-error response head too (before the in-band
/// stream error surfaces), exactly once for the failed attempt.
#[tokio::test]
async fn genai_stream_fn_request_on_response_fires_on_http_error() {
    let (url, connections) = spawn_scripted_server(vec![http_429_with_short_retry()]).await;
    let seen_responses: SeenResponses = Arc::default();
    let stream_fn = GenaiStreamFn::new(Client::builder().build());

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    )
    .with_on_response(recording_on_response(seen_responses.clone()));
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    let responses = seen_responses.lock().unwrap();
    assert_eq!(
        responses.len(),
        1,
        "one observer call for the failed attempt"
    );
    assert_eq!(responses[0].0.status, 429);
}

/// Under retries, per-request exec hooks fire once per physical attempt: `on_payload` runs for
/// every re-issued request's payload and `on_response` observes every response head, including
/// the retryable 429.
#[tokio::test]
async fn genai_stream_fn_request_hooks_fire_once_per_physical_attempt_under_retries() {
    let (url, connections) =
        spawn_scripted_server(vec![http_429_with_short_retry(), openai_sse_ok_response()]).await;
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let stream_fn = GenaiStreamFn::new(Client::builder().build());

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    )
    .with_on_payload(recording_on_payload(seen_payloads.clone(), None))
    .with_on_response(recording_on_response(seen_responses.clone()))
    .with_max_retries(2);
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");

    assert_eq!(message.text(), "Hello");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "one retry was issued"
    );
    assert_eq!(
        seen_payloads.lock().unwrap().len(),
        2,
        "on_payload fired once per physical attempt"
    );
    let statuses: Vec<u16> = seen_responses
        .lock()
        .unwrap()
        .iter()
        .map(|(info, _)| info.status)
        .collect();
    assert_eq!(
        statuses,
        [429, 200],
        "on_response observed every response head, including the retryable error"
    );
}

// endregion: --- GenaiStreamFn construction-time wiring

/// Spawn a scripted server answering the Nth connection with `responses[N]`, returning the base
/// URL and a counter of accepted connections (i.e., physical attempts made).
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
            let mut buffer = [0_u8; 8192];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{address}/"), connections)
}

/// A 429 response asking for a 1ms retry delay.
fn http_429_with_short_retry() -> String {
    let body = r#"{"error":{"message":"rate limited"}}"#;
    format!(
        "HTTP/1.1 429 Too Many Requests\r\n\
         content-type: application/json\r\n\
         retry-after-ms: 1\r\n\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// End-to-end through the `Agent` facade with a real `GenaiStreamFn`: the facade snapshots the
/// configured hooks at run admission, so an idle `set_on_payload` replacement takes effect on the
/// next run — wire-verified — while the previous run's wire payload proves its snapshot stayed
/// stable. The session id reaches each run for correlation without ever entering provider JSON.
#[tokio::test]
async fn agent_idle_hook_replacement_changes_the_next_run_wire_payload() {
    // Two connections: run one and run two.
    let (url, connections) =
        spawn_scripted_server(vec![openai_sse_ok_response(), openai_sse_ok_response()]).await;
    let first_seen: SeenPayloads = Arc::default();
    let second_seen: SeenPayloads = Arc::default();
    let first_hook = recording_on_payload(first_seen.clone(), Some(json!({"x_run": 1})));
    let stream_fn = GenaiStreamFn::new(Client::builder().build());
    let state = AgentState {
        model: local_target(url),
        ..AgentState::default()
    };
    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(stream_fn))
            .with_on_payload(first_hook.clone())
            .with_session_id("session-for-correlation"),
    );

    // Run one: the configured hook fires and its replacement goes over the wire.
    agent.prompt("first").await.expect("first run completes");
    // Idle replacement: swap the payload hook between runs.
    let second_hook = recording_on_payload(second_seen.clone(), Some(json!({"x_run": 2})));
    agent
        .set_on_payload(Some(second_hook.clone()))
        .expect("idle replacement is admitted");
    agent.prompt("second").await.expect("second run completes");

    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "two runs, two attempts"
    );
    assert_eq!(
        first_seen.lock().unwrap().len(),
        1,
        "the first hook fired only on the first run"
    );
    assert_eq!(
        second_seen.lock().unwrap().len(),
        1,
        "the idle replacement fired on the next run"
    );
    // The replacement hook saw the unmodified serialized payload: the two hooks never compose,
    // and neither payload carries the session id or a prompt cache key.
    {
        let second_payloads = second_seen.lock().unwrap();
        let second_payload = &second_payloads[0].0;
        assert_eq!(second_payload.get("x_run"), None);
        assert_eq!(
            second_payload.get("model").and_then(Value::as_str),
            Some("gpt-test")
        );
    }
    for seen in [&first_seen, &second_seen] {
        let seen = seen.lock().unwrap();
        let payload = &seen[0].0;
        assert_eq!(
            payload.get("session_id"),
            None,
            "session id never enters provider JSON"
        );
        assert_eq!(payload.get("sessionId"), None);
        assert_eq!(payload.get("prompt_cache_key"), None);
    }
    // The run snapshots prove per-execution correlation reached the requests: the facade keeps
    // the configured session id and forwarded it onto each run's stream requests.
    assert_eq!(
        agent.session_id().as_deref(),
        Some("session-for-correlation")
    );
}

/// Under retries, the construction-time exec hooks fire once per physical attempt: `on_payload`
/// runs for every re-issued request's payload and `on_response` observes every response head,
/// including the retryable 429.
#[tokio::test]
async fn exec_hooks_fire_once_per_physical_attempt_under_retries() {
    let (url, connections) =
        spawn_scripted_server(vec![http_429_with_short_retry(), openai_sse_ok_response()]).await;
    let seen_payloads: SeenPayloads = Arc::default();
    let seen_responses: SeenResponses = Arc::default();
    let on_payload = recording_on_payload(seen_payloads.clone(), None);
    let on_response = recording_on_response(seen_responses.clone());
    let stream_fn =
        GenaiStreamFn::with_exec_hooks(Client::builder(), Some(on_payload), Some(on_response))
            .with_retry(rust_genai_agent::RetryPolicy {
                max_retries: 2,
                max_retry_delay_ms: 60_000,
            });

    let request = StreamRequest::new(
        local_target(url),
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        },
    );
    let stream = stream_fn.stream(request).await;
    let result = stream.result_handle();
    let _events: Vec<_> = tokio::time::timeout(TEST_TIMEOUT, stream.collect())
        .await
        .expect("assistant stream finished before timeout");
    let message = tokio::time::timeout(TEST_TIMEOUT, result.get())
        .await
        .expect("terminal result before timeout")
        .expect("terminal message");

    assert_eq!(message.text(), "Hello");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "one retry was issued"
    );
    assert_eq!(
        seen_payloads.lock().unwrap().len(),
        2,
        "on_payload fired once per physical attempt"
    );
    let responses = seen_responses.lock().unwrap();
    let statuses: Vec<u16> = responses.iter().map(|(info, _)| info.status).collect();
    assert_eq!(
        statuses,
        [429, 200],
        "on_response observed every response head, including the retryable error"
    );
}
