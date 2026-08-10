//! Fresh-consumer fixture for DIST-01 (scripts/check-distribution.sh).
//!
//! Built and tested against the *extracted crate archives* of `rust-genai-agent` and the pinned
//! `genai` fork — never the sibling source checkouts — to prove the packaged artifacts carry the
//! fork-only APIs this crate requires and that a consumer can drive them end to end:
//!
//! - `genai::ExecOptions` request-level exec-hook overrides (inherit / replace / disable),
//! - `rust_genai_agent::GenaiStreamFn` honoring per-request `on_payload` / `on_response` hooks and
//!   per-request retry overrides.
//!
//! `main` stays offline (construction only); the behavioral proof is the `#[cfg(test)]` module,
//! which `cargo test` runs against a local one-shot capture server.

use genai::{Client, ExecOptions};
use rust_genai_agent::{GenaiStreamFn, RetryPolicy};

fn main() {
    // Construction-only smoke: the packaged crates expose the fork-only surface.
    let client = Client::builder().build();
    let stream_fn = GenaiStreamFn::new(client).with_retry(RetryPolicy::default());
    let exec_options = ExecOptions::new().without_response_observer();
    println!(
        "fresh-consumer ok: GenaiStreamFn retry={:?}, exec_options={:?}",
        stream_fn.retry, exec_options
    );
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use genai::adapter::AdapterKind;
    use genai::chat::ChatMessage;
    use genai::resolver::{AuthData, Endpoint};
    use genai::{Client, ModelIden, ModelSpec, ServiceTarget};
    use rust_genai_agent::{
        GenaiStreamFn, LlmContext, OnPayloadHook, OnResponseHook, RetryPolicy, StopReason,
        StreamFn, StreamRequest, StreamResponseInfo,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Spawn a scripted server answering the Nth accepted connection with `responses[N]`.
    /// Returns the base URL, a per-connection request-body log, and a connection counter.
    async fn spawn_scripted_server(
        responses: Vec<String>,
    ) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local scripted server");
        let address = listener.local_addr().expect("scripted server address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let connections = Arc::new(AtomicUsize::new(0));
        let bodies_bg = bodies.clone();
        let connections_bg = connections.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                connections_bg.fetch_add(1, Ordering::SeqCst);
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
                    if let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
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
                bodies_bg.lock().unwrap().push(body);
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}/"), bodies, connections)
    }

    fn sse_ok_response() -> String {
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

    fn http_429_short_retry() -> String {
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

    fn local_target(url: String) -> ModelSpec {
        ModelSpec::from_target(ServiceTarget {
            endpoint: Endpoint::from_owned(url),
            auth: AuthData::from_single("test-key"),
            model: ModelIden::new(AdapterKind::OpenAI, "gpt-test"),
        })
    }

    fn llm_context() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: vec![ChatMessage::user("Why is the sky red?")],
            tools: Vec::new(),
        }
    }

    /// End-to-end proof that the packaged fork + packaged agent crate deliver the execution
    /// seam: per-request hooks replace the construction-time hook without composing, fire once
    /// per physical attempt under a per-request retry override, and the session id never enters
    /// provider JSON.
    #[tokio::test]
    async fn request_hooks_and_retry_overrides_work_from_packaged_archives() {
        let (url, bodies, connections) =
            spawn_scripted_server(vec![http_429_short_retry(), sse_ok_response()]).await;

        // Construction-time hook: must be replaced (not composed) by the per-request hook.
        let construction_payloads = Arc::new(AtomicUsize::new(0));
        let construction_hook: OnPayloadHook = Arc::new({
            let construction_payloads = construction_payloads.clone();
            move |_payload, _model| {
                let construction_payloads = construction_payloads.clone();
                Box::pin(async move {
                    construction_payloads.fetch_add(1, Ordering::SeqCst);
                    Some(json!({"x_construction": true}))
                })
            }
        });
        let stream_fn = GenaiStreamFn::with_exec_hooks(Client::builder(), Some(construction_hook), None)
            .with_retry(RetryPolicy {
                max_retries: 0,
                max_retry_delay_ms: 60_000,
            });

        let request_payloads: Arc<Mutex<Vec<Value>>> = Arc::default();
        let request_hook: OnPayloadHook = Arc::new({
            let request_payloads = request_payloads.clone();
            move |payload, _model| {
                let request_payloads = request_payloads.clone();
                Box::pin(async move {
                    request_payloads.lock().unwrap().push(payload);
                    Some(json!({"x_request": true}))
                })
            }
        });
        let responses_seen: Arc<Mutex<Vec<StreamResponseInfo>>> = Arc::default();
        let response_hook: OnResponseHook = Arc::new({
            let responses_seen = responses_seen.clone();
            move |info, _model| {
                let responses_seen = responses_seen.clone();
                Box::pin(async move {
                    responses_seen.lock().unwrap().push(info);
                })
            }
        });

        let request = StreamRequest::new(local_target(url), llm_context())
            .with_on_payload(request_hook)
            .with_on_response(response_hook)
            .with_session_id("consumer-session-1")
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
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(connections.load(Ordering::SeqCst), 2, "the per-request max_retries drove one retry");

        // Exactly one hook per channel per physical attempt; construction hook never fired.
        assert_eq!(construction_payloads.load(Ordering::SeqCst), 0, "replaced, never composed");
        assert_eq!(request_payloads.lock().unwrap().len(), 2, "once per attempt");
        let statuses: Vec<u16> = responses_seen
            .lock()
            .unwrap()
            .iter()
            .map(|info| info.status)
            .collect();
        assert_eq!(statuses, [429, 200], "every response head observed");

        // The wire carried only the request replacement, and no session/cache-key field.
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        for body in bodies.iter() {
            let wire_json: Value = serde_json::from_str(body).expect("wire body is JSON");
            assert_eq!(wire_json, json!({"x_request": true}), "full replacement, no composition");
        }
    }
}
