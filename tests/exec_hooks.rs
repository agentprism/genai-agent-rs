//! `on_payload`/`on_response` exec-hook plumbing and wiring coverage.
//!
//! The loop and the `Agent` facade forward the hooks as handles onto each [`StreamRequest`];
//! honoring them is a stream-function concern. `GenaiStreamFn` applies construction-time hooks
//! through the genai fork's client-level interceptors, verified here against a local one-shot
//! capture server (the fork's own offline test pattern) because the fork's `intercept`/`observe`
//! entry points are crate-private and cannot be driven directly.

#![cfg(feature = "testing")]

use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::ChatMessage;
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ModelSpec, ServiceTarget};
use rust_genai_agent::testing::{EventRecorder, MockStreamFn, fixtures, script};
use rust_genai_agent::{
    Agent, AgentConfig, AgentContext, AgentLoopConfig, GenaiStreamFn, LlmContext, OnPayloadHook,
    OnResponseHook, StreamFn, StreamRequest, StreamResponseInfo, default_convert_to_llm,
    run_agent_loop,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

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

/// The per-request `StreamRequest` hooks are documented as ignored by `GenaiStreamFn`
/// (client-level fork interceptors cannot vary per request); the wire body and observer state
/// prove nothing fires when only the request carries hooks.
#[tokio::test]
async fn genai_stream_fn_ignores_request_level_exec_hooks() {
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
        Some(json!({"never": "sent"})),
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

    assert!(seen_payloads.lock().unwrap().is_empty());
    assert!(seen_responses.lock().unwrap().is_empty());
    let wire_body = tokio::time::timeout(TEST_TIMEOUT, body_rx)
        .await
        .expect("captured request before timeout")
        .expect("capture server recorded a body");
    let wire_json: Value = serde_json::from_str(&wire_body).expect("wire body is JSON");
    assert_eq!(wire_json.get("never"), None);
    assert_eq!(
        wire_json.get("model").and_then(Value::as_str),
        Some("gpt-test")
    );
}

// endregion: --- GenaiStreamFn construction-time wiring
