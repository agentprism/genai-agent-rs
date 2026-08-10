//! Offline tests for the per-request exec hooks (`PayloadInterceptor` / `ResponseObserver`)
//! on the chat exec paths (`exec_chat` and `exec_chat_stream`), using a local one-shot
//! HTTP server (no network, no provider keys).

use crate::adapter::AdapterKind;
use crate::chat::{ChatRequest, ChatStreamEvent};
use crate::resolver::{AuthData, Endpoint};
use crate::{Client, Error, ModelIden, ResponseObserver, ServiceTarget};
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn test_client_exec_chat_stream_payload_interceptor_replaces_wire_payload() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let seen: Arc<Mutex<Option<(ModelIden, Value)>>> = Arc::new(Mutex::new(None));
	let seen_clone = seen.clone();
	let client = Client::builder()
		.with_payload_interceptor_fn(move |model_iden: ModelIden, mut payload: Value| -> Option<Value> {
			*seen_clone.lock().unwrap() = Some((model_iden, payload.clone()));
			payload["x_intercepted"] = json!(true);
			Some(payload)
		})
		.build();

	// -- Exec
	let chat_res = client
		.exec_chat_stream(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
		)
		.await?;
	let content = support_collect_content(chat_res).await?;

	// -- Check
	assert_eq!(content, "Hello");
	// The interceptor saw the target ModelIden and the original serialized payload.
	let (seen_model, seen_payload) = seen.lock().unwrap().take().ok_or("Interceptor should have been called")?;
	assert_eq!(seen_model.adapter_kind, AdapterKind::OpenAI);
	assert_eq!(&*seen_model.model_name, "gpt-test");
	assert_eq!(seen_payload.get("model").and_then(|v| v.as_str()), Some("gpt-test"));
	assert_eq!(seen_payload.get("x_intercepted"), None);
	// The replacement payload is what actually went over the wire.
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_intercepted"), Some(&json!(true)));
	assert_eq!(wire_json.get("model").and_then(|v| v.as_str()), Some("gpt-test"));

	Ok(())
}

#[tokio::test]
async fn test_client_exec_chat_stream_response_observer_on_success() -> Result<()> {
	// -- Setup & Fixtures
	let (url, _body_rx) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let (observed, order) = support_new_observer_state();
	let client = Client::builder()
		.with_response_observer(support_async_observer(observed.clone(), order.clone()))
		.build();

	// -- Exec
	let mut chat_res = client
		.exec_chat_stream(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
		)
		.await?;
	// The HTTP send is lazy — nothing observed before the stream is polled.
	assert!(
		observed.lock().unwrap().is_none(),
		"Observer should not fire before first poll"
	);
	let mut content = String::new();
	while let Some(event) = chat_res.stream.next().await {
		if let ChatStreamEvent::Chunk(chunk) = event? {
			if content.is_empty() {
				order.lock().unwrap().push("first-chunk".to_string());
			}
			content.push_str(&chunk.content);
		}
	}

	// -- Check
	assert_eq!(content, "Hello");
	let (model_iden, status, headers) = observed.lock().unwrap().take().ok_or("Observer should have fired")?;
	assert_eq!(&*model_iden.model_name, "gpt-test");
	assert_eq!(status, StatusCode::OK);
	assert_eq!(
		headers.get("x-obs-test").and_then(|v| v.to_str().ok()),
		Some("obs-value")
	);
	// The observer fired on the response head, before the stream body was consumed.
	assert_eq!(
		*order.lock().unwrap(),
		vec!["observer".to_string(), "first-chunk".to_string()]
	);

	Ok(())
}

#[tokio::test]
async fn test_client_exec_chat_stream_response_observer_on_http_error() -> Result<()> {
	// -- Setup & Fixtures
	let error_body = r#"{"error":{"message":"rate limited"}}"#;
	let raw_response = format!(
		"HTTP/1.1 429 Too Many Requests\r\n\
		content-type: application/json\r\n\
		retry-after: 2\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{error_body}",
		error_body.len()
	);
	let (url, _body_rx) = support_spawn_capture_server(raw_response).await?;
	let (observed, _order) = support_new_observer_state();
	let client = Client::builder()
		.with_response_observer(support_async_observer(observed.clone(), _order.clone()))
		.build();

	// -- Exec
	let mut chat_res = client
		.exec_chat_stream(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
		)
		.await?;
	let mut stream_err: Option<Error> = None;
	while let Some(event) = chat_res.stream.next().await {
		if let Err(err) = event {
			stream_err = Some(err);
			break;
		}
	}

	// -- Check
	// The observer fired on the failing response head.
	let (_model_iden, status, headers) = observed.lock().unwrap().take().ok_or("Observer should have fired")?;
	assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
	assert_eq!(headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("2"));
	// AND the returned HttpError still carries the response headers (block-1 behavior).
	let stream_err = stream_err.ok_or("Stream should have yielded an error")?;
	let Error::WebStream { error, .. } = stream_err else {
		return Err(format!("Should be Error::WebStream, but was: {stream_err}").into());
	};
	let http_err = error
		.downcast::<Error>()
		.map_err(|err| format!("Error should downcast to genai Error, but was: {err}"))?;
	match *http_err {
		Error::HttpError {
			status, body, headers, ..
		} => {
			assert_eq!(status.as_u16(), 429);
			assert_eq!(body, error_body);
			assert_eq!(headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("2"));
		}
		other => return Err(format!("Should be Error::HttpError, but was: {other}").into()),
	}

	Ok(())
}

#[tokio::test]
async fn test_client_exec_chat_stream_no_hooks_regression() -> Result<()> {
	// -- Setup & Fixtures
	let (url_baseline, body_rx_baseline) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let (url_noop, body_rx_noop) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let client_baseline = Client::builder().build();
	// A `None`-returning interceptor must keep the payload unchanged (byte-identical wire body).
	let client_noop = Client::builder()
		.with_payload_interceptor_fn(|_model_iden: ModelIden, _payload: Value| -> Option<Value> { None })
		.build();

	// -- Exec
	let chat_req = ChatRequest::from_user("Why is the sky red?");
	let res_baseline = client_baseline
		.exec_chat_stream(support_target(&url_baseline), chat_req.clone(), None)
		.await?;
	let content_baseline = support_collect_content(res_baseline).await?;
	let res_noop = client_noop.exec_chat_stream(support_target(&url_noop), chat_req, None).await?;
	let content_noop = support_collect_content(res_noop).await?;

	// -- Check
	assert_eq!(content_baseline, "Hello");
	assert_eq!(content_noop, "Hello");
	let body_baseline = body_rx_baseline.await?;
	let body_noop = body_rx_noop.await?;
	assert_eq!(body_baseline, body_noop, "Wire payload must be byte-identical");

	Ok(())
}

#[tokio::test]
async fn test_client_exec_chat_payload_interceptor_and_observer() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_json_ok_response()).await?;
	let (observed, _order) = support_new_observer_state();
	let client = Client::builder()
		.with_payload_interceptor_fn(|_model_iden: ModelIden, mut payload: Value| -> Option<Value> {
			payload["x_intercepted"] = json!(true);
			Some(payload)
		})
		.with_response_observer(support_async_observer(observed.clone(), _order.clone()))
		.build();

	// -- Exec
	let chat_res = client
		.exec_chat(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
		)
		.await?;

	// -- Check
	assert_eq!(chat_res.first_text(), Some("Hello"));
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_intercepted"), Some(&json!(true)));
	let (model_iden, status, headers) = observed.lock().unwrap().take().ok_or("Observer should have fired")?;
	assert_eq!(&*model_iden.model_name, "gpt-test");
	assert_eq!(status, StatusCode::OK);
	assert_eq!(
		headers.get("x-obs-test").and_then(|v| v.to_str().ok()),
		Some("obs-value")
	);

	Ok(())
}

#[tokio::test]
async fn test_client_exec_chat_response_observer_on_http_error() -> Result<()> {
	// -- Setup & Fixtures
	let error_body = r#"{"error":{"message":"boom"}}"#;
	let raw_response = format!(
		"HTTP/1.1 500 Internal Server Error\r\n\
		content-type: application/json\r\n\
		x-obs-test: obs-value\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{error_body}",
		error_body.len()
	);
	let (url, _body_rx) = support_spawn_capture_server(raw_response).await?;
	let (observed, _order) = support_new_observer_state();
	let client = Client::builder()
		.with_response_observer(support_async_observer(observed.clone(), _order.clone()))
		.build();

	// -- Exec
	let res = client
		.exec_chat(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
		)
		.await;

	// -- Check
	// The observer fired on the failing response head, before the error body was consumed.
	let (_model_iden, status, headers) = observed.lock().unwrap().take().ok_or("Observer should have fired")?;
	assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
	assert_eq!(
		headers.get("x-obs-test").and_then(|v| v.to_str().ok()),
		Some("obs-value")
	);
	// And the call still returns the regular web error.
	let err = res.err().ok_or("exec_chat should have failed")?;
	let Error::WebModelCall { webc_error, .. } = err else {
		return Err(format!("Should be Error::WebModelCall, but was: {err}").into());
	};
	match webc_error {
		crate::webc::Error::ResponseFailedStatus { status, body, .. } => {
			assert_eq!(status.as_u16(), 500);
			assert_eq!(body, error_body);
		}
		other => return Err(format!("Should be ResponseFailedStatus, but was: {other}").into()),
	}

	Ok(())
}

// region:    --- Request-level ExecOptions overrides

use crate::client::{ExecHookOverride, ExecOptions, PayloadInterceptor};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counting sync payload interceptor that tags the payload with a marker field.
fn support_counting_interceptor(count: Arc<AtomicUsize>, marker: &'static str) -> PayloadInterceptor {
	PayloadInterceptor::from_interceptor_fn(move |_model_iden: ModelIden, mut payload: Value| -> Option<Value> {
		count.fetch_add(1, Ordering::SeqCst);
		payload[marker] = json!(true);
		Some(payload)
	})
}

/// Counting sync response observer that records every observed status, in order.
fn support_counting_observer(count: Arc<AtomicUsize>, statuses: Arc<Mutex<Vec<u16>>>) -> ResponseObserver {
	ResponseObserver::from_observer_fn(move |_model_iden: ModelIden, status: StatusCode, _headers: HeaderMap| {
		count.fetch_add(1, Ordering::SeqCst);
		statuses.lock().unwrap().push(status.as_u16());
	})
}

#[tokio::test]
async fn test_exec_chat_request_interceptor_replaces_construction_default_without_composing() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_json_ok_response()).await?;
	let construction_calls = Arc::new(AtomicUsize::new(0));
	let request_calls = Arc::new(AtomicUsize::new(0));
	let client = Client::builder()
		.with_payload_interceptor(support_counting_interceptor(
			construction_calls.clone(),
			"x_construction",
		))
		.build();
	let exec_options =
		ExecOptions::new().with_payload_interceptor(support_counting_interceptor(request_calls.clone(), "x_request"));

	// -- Exec
	let chat_res = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;

	// -- Check
	assert_eq!(chat_res.first_text(), Some("Hello"));
	assert_eq!(
		construction_calls.load(Ordering::SeqCst),
		0,
		"construction hook must not fire on a replaced channel"
	);
	assert_eq!(
		request_calls.load(Ordering::SeqCst),
		1,
		"exactly one request-hook invocation for the attempt"
	);
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_request"), Some(&json!(true)));
	assert_eq!(
		wire_json.get("x_construction"),
		None,
		"the request replacement never composes with the construction default"
	);

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_request_disable_suppresses_construction_interceptor() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_json_ok_response()).await?;
	let construction_calls = Arc::new(AtomicUsize::new(0));
	let client = Client::builder()
		.with_payload_interceptor(support_counting_interceptor(
			construction_calls.clone(),
			"x_construction",
		))
		.build();
	let exec_options = ExecOptions::new().without_payload_interceptor();

	// -- Exec
	let chat_res = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;

	// -- Check
	assert_eq!(chat_res.first_text(), Some("Hello"));
	assert_eq!(
		construction_calls.load(Ordering::SeqCst),
		0,
		"disable suppresses the construction hook"
	);
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_construction"), None, "wire payload stays pristine");
	assert_eq!(wire_json.get("model").and_then(|v| v.as_str()), Some("gpt-test"));

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_inherit_exec_options_fire_construction_hooks_once() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_json_ok_response()).await?;
	let interceptor_calls = Arc::new(AtomicUsize::new(0));
	let observer_calls = Arc::new(AtomicUsize::new(0));
	let observed_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let client = Client::builder()
		.with_payload_interceptor(support_counting_interceptor(
			interceptor_calls.clone(),
			"x_construction",
		))
		.with_response_observer(support_counting_observer(
			observer_calls.clone(),
			observed_statuses.clone(),
		))
		.build();
	// An explicit all-Inherit ExecOptions must behave exactly like the classic exec_chat.
	let exec_options = ExecOptions::new();

	// -- Exec
	let chat_res = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;

	// -- Check
	assert_eq!(chat_res.first_text(), Some("Hello"));
	assert_eq!(
		interceptor_calls.load(Ordering::SeqCst),
		1,
		"inherited interceptor fires exactly once"
	);
	assert_eq!(
		observer_calls.load(Ordering::SeqCst),
		1,
		"inherited observer fires exactly once"
	);
	assert_eq!(*observed_statuses.lock().unwrap(), vec![200]);
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_construction"), Some(&json!(true)));

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_request_observer_replaces_construction_on_http_error() -> Result<()> {
	// -- Setup & Fixtures
	let error_body = r#"{"error":{"message":"boom"}}"#;
	let raw_response = format!(
		"HTTP/1.1 500 Internal Server Error\r\n\
		content-type: application/json\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{error_body}",
		error_body.len()
	);
	let (url, _body_rx) = support_spawn_capture_server(raw_response).await?;
	let construction_calls = Arc::new(AtomicUsize::new(0));
	let construction_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let request_calls = Arc::new(AtomicUsize::new(0));
	let request_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let client = Client::builder()
		.with_response_observer(support_counting_observer(
			construction_calls.clone(),
			construction_statuses.clone(),
		))
		.build();
	let exec_options = ExecOptions::new().with_response_observer(support_counting_observer(
		request_calls.clone(),
		request_statuses.clone(),
	));

	// -- Exec
	let res = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await;

	// -- Check
	assert!(res.is_err(), "the 500 still surfaces as the call error");
	assert_eq!(
		construction_calls.load(Ordering::SeqCst),
		0,
		"construction observer must not fire on a replaced channel"
	);
	assert_eq!(
		request_calls.load(Ordering::SeqCst),
		1,
		"exactly one request-observer invocation for the failed attempt"
	);
	assert_eq!(
		*request_statuses.lock().unwrap(),
		vec![500],
		"the request observer fires on HTTP errors too"
	);

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_stream_request_observer_replaces_construction_on_success() -> Result<()> {
	// -- Setup & Fixtures
	let (url, _body_rx) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let construction_calls = Arc::new(AtomicUsize::new(0));
	let construction_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let request_calls = Arc::new(AtomicUsize::new(0));
	let request_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let client = Client::builder()
		.with_response_observer(support_counting_observer(
			construction_calls.clone(),
			construction_statuses.clone(),
		))
		.build();
	let exec_options = ExecOptions::new().with_response_observer(support_counting_observer(
		request_calls.clone(),
		request_statuses.clone(),
	));

	// -- Exec
	let chat_res = client
		.exec_chat_stream_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;
	let content = support_collect_content(chat_res).await?;

	// -- Check
	assert_eq!(content, "Hello");
	assert_eq!(construction_calls.load(Ordering::SeqCst), 0);
	assert_eq!(
		request_calls.load(Ordering::SeqCst),
		1,
		"exactly one request-observer invocation for the attempt"
	);
	assert_eq!(*request_statuses.lock().unwrap(), vec![200]);

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_stream_request_disable_suppresses_construction_observer_on_http_error() -> Result<()> {
	// -- Setup & Fixtures
	let error_body = r#"{"error":{"message":"rate limited"}}"#;
	let raw_response = format!(
		"HTTP/1.1 429 Too Many Requests\r\n\
		content-type: application/json\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{error_body}",
		error_body.len()
	);
	let (url, _body_rx) = support_spawn_capture_server(raw_response).await?;
	let construction_calls = Arc::new(AtomicUsize::new(0));
	let construction_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let client = Client::builder()
		.with_response_observer(support_counting_observer(
			construction_calls.clone(),
			construction_statuses.clone(),
		))
		.build();
	let exec_options = ExecOptions::new().without_response_observer();

	// -- Exec
	let mut chat_res = client
		.exec_chat_stream_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;
	let mut stream_err: Option<Error> = None;
	while let Some(event) = chat_res.stream.next().await {
		if let Err(err) = event {
			stream_err = Some(err);
			break;
		}
	}

	// -- Check
	assert!(stream_err.is_some(), "the 429 still surfaces as a stream error");
	assert_eq!(
		construction_calls.load(Ordering::SeqCst),
		0,
		"disable suppresses the construction observer even on the error path"
	);

	Ok(())
}

#[tokio::test]
async fn test_exec_chat_stream_request_interceptor_without_construction_default() -> Result<()> {
	// -- Setup & Fixtures
	let (url, body_rx) = support_spawn_capture_server(support_sse_ok_response()).await?;
	let request_calls = Arc::new(AtomicUsize::new(0));
	let client = Client::builder().build();
	let request_calls_hook = request_calls.clone();
	let exec_options = ExecOptions::new().with_payload_interceptor_fn(
		move |_model_iden: ModelIden, mut payload: Value| -> Option<Value> {
			request_calls_hook.fetch_add(1, Ordering::SeqCst);
			payload["x_request"] = json!(true);
			Some(payload)
		},
	);

	// -- Exec
	let chat_res = client
		.exec_chat_stream_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&exec_options),
		)
		.await?;
	let content = support_collect_content(chat_res).await?;

	// -- Check
	assert_eq!(content, "Hello");
	assert_eq!(request_calls.load(Ordering::SeqCst), 1);
	let wire_body = body_rx.await?;
	let wire_json: Value = serde_json::from_str(&wire_body)?;
	assert_eq!(wire_json.get("x_request"), Some(&json!(true)));

	Ok(())
}

/// Simulated retry loop: the caller re-issues the request after a retryable 429, re-resolving the
/// exec options per attempt. Each attempt resolves and fires its hooks independently and exactly
/// once: attempt one (disable payload, replace response) fires the request observer on the 429
/// only; attempt two (replace payload, inherit response) fires the request interceptor and the
/// construction observer on the 200 only.
#[tokio::test]
async fn test_request_exec_options_resolve_once_per_attempt_across_manual_retries() -> Result<()> {
	// -- Setup & Fixtures
	let error_body = r#"{"error":{"message":"rate limited"}}"#;
	let raw_429 = format!(
		"HTTP/1.1 429 Too Many Requests\r\n\
		content-type: application/json\r\n\
		retry-after-ms: 1\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{error_body}",
		error_body.len()
	);
	let (url, connections) = support_spawn_scripted_server(vec![raw_429, support_json_ok_response()]).await?;

	let construction_interceptor_calls = Arc::new(AtomicUsize::new(0));
	let construction_observer_calls = Arc::new(AtomicUsize::new(0));
	let construction_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let client = Client::builder()
		.with_payload_interceptor(support_counting_interceptor(
			construction_interceptor_calls.clone(),
			"x_construction",
		))
		.with_response_observer(support_counting_observer(
			construction_observer_calls.clone(),
			construction_statuses.clone(),
		))
		.build();

	let request_interceptor_calls = Arc::new(AtomicUsize::new(0));
	let request_observer_calls = Arc::new(AtomicUsize::new(0));
	let request_statuses: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
	let attempt_one_options =
		ExecOptions::new()
			.without_payload_interceptor()
			.with_response_observer(support_counting_observer(
				request_observer_calls.clone(),
				request_statuses.clone(),
			));
	let attempt_two_options = ExecOptions::new().with_payload_interceptor(support_counting_interceptor(
		request_interceptor_calls.clone(),
		"x_request",
	));

	// -- Exec: attempt one fails with a retryable 429.
	let attempt_one = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&attempt_one_options),
		)
		.await;
	assert!(attempt_one.is_err(), "attempt one surfaces the 429");
	// -- Exec: the caller's retry loop re-issues the request with fresh per-attempt options.
	let attempt_two = client
		.exec_chat_with_exec_options(
			support_target(&url),
			ChatRequest::from_user("Why is the sky red?"),
			None,
			Some(&attempt_two_options),
		)
		.await?;

	// -- Check
	assert_eq!(attempt_two.first_text(), Some("Hello"));
	assert_eq!(
		connections.load(Ordering::SeqCst),
		2,
		"two physical HTTP attempts were made"
	);
	assert_eq!(
		construction_interceptor_calls.load(Ordering::SeqCst),
		0,
		"construction interceptor: disabled on attempt one, replaced on attempt two"
	);
	assert_eq!(
		request_interceptor_calls.load(Ordering::SeqCst),
		1,
		"request interceptor fires exactly once, on the attempt that installed it"
	);
	assert_eq!(
		request_observer_calls.load(Ordering::SeqCst),
		1,
		"request observer fires exactly once, on the attempt that installed it"
	);
	assert_eq!(
		*request_statuses.lock().unwrap(),
		vec![429],
		"request observer saw the failed attempt's head"
	);
	assert_eq!(
		construction_observer_calls.load(Ordering::SeqCst),
		1,
		"construction observer fires only on the attempt that inherited it"
	);
	assert_eq!(*construction_statuses.lock().unwrap(), vec![200]);

	Ok(())
}

/// ExecHookOverride resolution unit checks: inherit/replace/disable are per-channel exact.
#[test]
fn test_exec_hook_override_resolution_states() {
	let default_hook = support_counting_interceptor(Arc::new(AtomicUsize::new(0)), "x_default");
	let request_hook = support_counting_interceptor(Arc::new(AtomicUsize::new(0)), "x_request");

	let inherit: ExecHookOverride<PayloadInterceptor> = ExecHookOverride::Inherit;
	assert!(inherit.resolve(Some(&default_hook)).is_some());
	assert!(inherit.resolve(None).is_none());

	let replace: ExecHookOverride<PayloadInterceptor> = ExecHookOverride::Replace(request_hook);
	assert!(replace.resolve(Some(&default_hook)).is_some());
	assert!(
		replace.resolve(None).is_some(),
		"replace installs the hook even without a default"
	);

	let disable: ExecHookOverride<PayloadInterceptor> = ExecHookOverride::Disable;
	assert!(disable.resolve(Some(&default_hook)).is_none());
	assert!(disable.resolve(None).is_none());

	// Default ExecOptions is all-inherit.
	let options = ExecOptions::new();
	assert!(matches!(options.payload_interceptor, ExecHookOverride::Inherit));
	assert!(matches!(options.response_observer, ExecHookOverride::Inherit));
}

// endregion: --- Request-level ExecOptions overrides

// region:    --- Support

/// Builds a fully-resolved ServiceTarget pointing at the local test server (OpenAI adapter).
fn support_target(url: &str) -> ServiceTarget {
	ServiceTarget {
		endpoint: Endpoint::from_owned(url.to_string()),
		auth: AuthData::from_single("test-key"),
		model: ModelIden::new(AdapterKind::OpenAI, "gpt-test"),
	}
}

/// Raw SSE success response with one content chunk (OpenAI chat completions shape).
fn support_sse_ok_response() -> String {
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

/// Raw JSON success response (OpenAI chat completions shape) for the non-streaming path.
fn support_json_ok_response() -> String {
	let body = r#"{"id":"chatcmpl-1","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
	format!(
		"HTTP/1.1 200 OK\r\n\
		content-type: application/json\r\n\
		x-obs-test: obs-value\r\n\
		content-length: {}\r\n\
		connection: close\r\n\
		\r\n\
		{body}",
		body.len()
	)
}

type ObservedState = Arc<Mutex<Option<(ModelIden, StatusCode, HeaderMap)>>>;
type OrderState = Arc<Mutex<Vec<String>>>;

fn support_new_observer_state() -> (ObservedState, OrderState) {
	(Arc::new(Mutex::new(None)), Arc::new(Mutex::new(Vec::new())))
}

/// Builds an async ResponseObserver that records the observed (model, status, headers) and
/// appends "observer" to the order log (to assert it fired before body consumption).
fn support_async_observer(observed: ObservedState, order: OrderState) -> ResponseObserver {
	ResponseObserver::from_observer_async_fn(
		move |model_iden: ModelIden,
		      status: StatusCode,
		      headers: HeaderMap|
		      -> Pin<Box<dyn Future<Output = ()> + Send>> {
			let observed = observed.clone();
			let order = order.clone();
			Box::pin(async move {
				*observed.lock().unwrap() = Some((model_iden, status, headers));
				order.lock().unwrap().push("observer".to_string());
			})
		},
	)
}

/// Consumes the chat stream and concatenates the text chunks.
async fn support_collect_content(mut chat_res: crate::chat::ChatStreamResponse) -> Result<String> {
	let mut content = String::new();
	while let Some(event) = chat_res.stream.next().await {
		if let ChatStreamEvent::Chunk(chunk) = event? {
			content.push_str(&chunk.content);
		}
	}
	Ok(content)
}

/// Spawns a one-shot HTTP server that reads the full request (headers + content-length body),
/// sends the captured request body through the returned channel, then answers with the given
/// raw HTTP response.
async fn support_spawn_capture_server(
	raw_response: String,
) -> Result<(String, tokio::sync::oneshot::Receiver<String>)> {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let addr = listener.local_addr()?;
	let (body_tx, body_rx) = tokio::sync::oneshot::channel::<String>();
	tokio::spawn(async move {
		if let Ok((mut socket, _)) = listener.accept().await {
			// -- Read the full request: headers, then content-length body bytes.
			let mut buf: Vec<u8> = Vec::new();
			let mut chunk = [0u8; 4096];
			let body = loop {
				let Ok(n) = socket.read(&mut chunk).await else {
					break String::new();
				};
				if n == 0 {
					break String::new();
				}
				buf.extend_from_slice(&chunk[..n]);
				if let Some(header_end) = support_find_subslice(&buf, b"\r\n\r\n") {
					let headers_txt = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
					let content_length: usize = headers_txt
						.lines()
						.find_map(|line| line.strip_prefix("content-length:"))
						.and_then(|v| v.trim().parse().ok())
						.unwrap_or(0);
					let body_start = header_end + 4;
					if buf.len() >= body_start + content_length {
						break String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();
					}
				}
			};
			let _ = body_tx.send(body);
			let _ = socket.write_all(raw_response.as_bytes()).await;
			let _ = socket.shutdown().await;
		}
	});
	Ok((format!("http://{addr}/"), body_rx))
}

fn support_find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

/// Spawns a scripted server answering the Nth accepted connection with `responses[N]` (one shot
/// per connection). Returns the base URL and a counter of accepted connections, i.e., the number
/// of physical HTTP attempts made.
async fn support_spawn_scripted_server(responses: Vec<String>) -> Result<(String, Arc<AtomicUsize>)> {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let addr = listener.local_addr()?;
	let connections = Arc::new(AtomicUsize::new(0));
	let connections_bg = connections.clone();
	tokio::spawn(async move {
		for response in responses {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			connections_bg.fetch_add(1, Ordering::SeqCst);
			// Best-effort: read the request head before answering.
			let mut buf = [0u8; 8192];
			let _ = socket.read(&mut buf).await;
			let _ = socket.write_all(response.as_bytes()).await;
			let _ = socket.shutdown().await;
		}
	});
	Ok((format!("http://{addr}/"), connections))
}

// endregion: --- Support
