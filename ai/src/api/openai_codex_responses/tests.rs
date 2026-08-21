use super::*;
use crate::types::{
    ConstrainedSamplingConfig, GrammarVariants, ModelCost, ModelCostRates, ModelInput,
    OpenAIResponsesCompat, StrictPreference, TextContent, ToolConstrainedSampling,
    ToolResultMessage, ToolResultRole, UserContent, UserMessage, UserRole,
};
use futures::FutureExt;
use futures::SinkExt;
use futures::future::BoxFuture;
use futures::stream;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

#[derive(Clone)]
struct ResponseSpec {
    status: u16,
    status_text: String,
    headers: BTreeMap<String, String>,
    chunks: Vec<Result<Vec<u8>, String>>,
    stay_open: bool,
    cancelled: Option<Arc<AtomicBool>>,
}

impl ResponseSpec {
    fn sse(payload: impl Into<String>) -> Self {
        Self {
            status: 200,
            status_text: "OK".to_owned(),
            headers: BTreeMap::from([("content-type".to_owned(), "text/event-stream".to_owned())]),
            chunks: vec![Ok(payload.into().into_bytes())],
            stay_open: false,
            cancelled: None,
        }
    }

    fn error(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            status_text: "error".to_owned(),
            headers: BTreeMap::new(),
            chunks: vec![Ok(body.into().into_bytes())],
            stay_open: false,
            cancelled: None,
        }
    }

    fn with_headers(mut self, headers: impl IntoIterator<Item = (String, String)>) -> Self {
        self.headers.extend(headers);
        self
    }

    fn stays_open(mut self) -> Self {
        self.stay_open = true;
        self
    }

    fn tracks_cancellation(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }
}

struct TrackedBody {
    inner: crate::types::ProviderBodyStream,
    cancelled: Arc<AtomicBool>,
}

impl futures::Stream for TrackedBody {
    type Item = Result<Vec<u8>, String>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct QueueFetch {
    responses: Mutex<VecDeque<ResponseSpec>>,
    requests: Mutex<Vec<ProviderHttpRequest>>,
    calls: AtomicUsize,
}

impl QueueFetch {
    fn new(responses: impl IntoIterator<Item = ResponseSpec>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            ..Self::default()
        })
    }

    fn requests(&self) -> Vec<ProviderHttpRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl crate::types::FetchFunction for QueueFetch {
    fn fetch(
        &self,
        request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let response = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        async move {
            let response = response.ok_or_else(|| "no queued response".to_owned())?;
            let body = stream::iter(response.chunks);
            let body = if response.stay_open {
                body.chain(stream::pending()).boxed()
            } else {
                body.boxed()
            };
            let body: crate::types::ProviderBodyStream = match response.cancelled {
                Some(cancelled) => Box::pin(TrackedBody {
                    inner: body,
                    cancelled,
                }),
                None => body,
            };
            Ok(ProviderHttpResponse {
                status: response.status,
                status_text: response.status_text,
                headers: response.headers,
                body: Some(body),
            })
        }
        .boxed()
    }
}

#[derive(Default)]
struct PendingHeaderFetch {
    signal: Mutex<Option<Arc<dyn crate::types::AbortSignal>>>,
}

impl crate::types::FetchFunction for PendingHeaderFetch {
    fn fetch(
        &self,
        request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        *self
            .signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = request.signal;
        futures::future::pending().boxed()
    }
}

struct NetworkThenFetch {
    response: Mutex<Option<ResponseSpec>>,
    calls: AtomicUsize,
}

struct NoBodyFetch;

impl crate::types::FetchFunction for NoBodyFetch {
    fn fetch(
        &self,
        _request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        async {
            Ok(ProviderHttpResponse {
                status: 200,
                status_text: "OK".to_owned(),
                headers: BTreeMap::new(),
                body: None,
            })
        }
        .boxed()
    }
}

impl NetworkThenFetch {
    fn new(response: ResponseSpec) -> Arc<Self> {
        Arc::new(Self {
            response: Mutex::new(Some(response)),
            calls: AtomicUsize::new(0),
        })
    }
}

impl crate::types::FetchFunction for NetworkThenFetch {
    fn fetch(
        &self,
        _request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let response = (call > 0)
            .then(|| {
                self.response
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            })
            .flatten();
        async move {
            if call == 0 {
                return Err("network disconnected".to_owned());
            }
            let response = response.ok_or_else(|| "no queued response".to_owned())?;
            let body = stream::iter(response.chunks);
            let body = if response.stay_open {
                body.chain(stream::pending()).boxed()
            } else {
                body.boxed()
            };
            let body: crate::types::ProviderBodyStream = match response.cancelled {
                Some(cancelled) => Box::pin(TrackedBody {
                    inner: body,
                    cancelled,
                }),
                None => body,
            };
            Ok(ProviderHttpResponse {
                status: response.status,
                status_text: response.status_text,
                headers: response.headers,
                body: Some(body),
            })
        }
        .boxed()
    }
}

#[derive(Default)]
struct ManualAbort {
    aborted: AtomicBool,
    notify: Notify,
}

impl ManualAbort {
    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl crate::types::AbortSignal for ManualAbort {
    fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        async move {
            while !self.is_aborted() {
                self.notify.notified().await;
            }
        }
        .boxed()
    }
}

fn token(account_id: &str) -> String {
    let payload = STANDARD.encode(
        serde_json::to_vec(&json!({JWT_CLAIM_PATH: {"chatgpt_account_id": account_id}}))
            .expect("payload"),
    );
    format!("aaa.{payload}.bbb")
}

/// Pins pi `src/api/openai-codex-responses.ts:1578-1587`: `atob` produces
/// a binary string before `JSON.parse`, and the claim is only truthiness-checked.
#[test]
fn account_id_uses_atob_binary_string_and_javascript_string_coercion() {
    let payload = STANDARD.encode(
        serde_json::to_vec(&json!({JWT_CLAIM_PATH: {"chatgpt_account_id": "é"}})).expect("payload"),
    );
    assert_eq!(
        extract_account_id(&format!("aaa.{payload}.bbb")).expect("account id"),
        "Ã©"
    );

    let numeric = STANDARD.encode(
        serde_json::to_vec(&json!({JWT_CLAIM_PATH: {"chatgpt_account_id": 7}})).expect("payload"),
    );
    assert_eq!(
        extract_account_id(&format!("aaa.{numeric}.bbb")).expect("account id"),
        "7"
    );
}

fn model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: "openai-codex-responses".into(),
        provider: "openai-codex".into(),
        base_url: "https://chatgpt.com/backend-api".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 400_000,
        max_tokens: 128_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn context(text: &str) -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".to_owned()),
        messages: vec![Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(text.to_owned()),
            timestamp: 1,
        }))],
        tools: None,
    }
}

fn terminal_payload(status: &str, end_turn: Option<bool>) -> String {
    let terminal_type = if status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let events = vec![
        json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}
        }),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"Hello"}),
        json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello"}]}
        }),
        json!({
            "type":terminal_type,
            "response":{
                "id":"resp_1",
                "status":status,
                "end_turn":end_turn,
                "incomplete_details":if status == "incomplete" { json!({"reason":"max_output_tokens"}) } else { Value::Null },
                "usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8,"input_tokens_details":{"cached_tokens":0}}
            }
        }),
    ];
    events
        .into_iter()
        .map(|event| format!("data: {event}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n"
}

fn sse_options(fetch: Arc<QueueFetch>) -> OpenAICodexResponsesOptions {
    let mut options = OpenAICodexResponsesOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    options.stream.request.fetch = Some(fetch);
    options.stream.transport = Some(Transport::Sse);
    options
}

fn decode_request(request: &ProviderHttpRequest) -> Value {
    let bytes = if request.headers.get("content-encoding").map(String::as_str) == Some("zstd") {
        zstd::stream::decode_all(Cursor::new(&request.body)).expect("zstd body")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("request JSON")
}

/// Port of pi `openai-codex-stream.test.ts:100-210`.
#[tokio::test]
async fn streams_sse_responses_into_assistant_message_event_stream() {
    let fetch = QueueFetch::new([ResponseSpec::sse(terminal_payload("completed", None))]);
    let mut events = stream(
        &model("gpt-5.1-codex"),
        &context("Say hello"),
        sse_options(fetch.clone()),
    );
    let mut kinds = Vec::new();
    while let Some(event) = events.next().await {
        kinds.push(match event {
            AssistantMessageEvent::Start => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::Done { ref message, .. } => {
                assert_eq!(message_text(message), Some("Hello"));
                "done"
            }
            _ => "other",
        });
    }
    assert_eq!(
        kinds,
        ["start", "text_start", "text_delta", "text_end", "done"]
    );
    let request = &fetch.requests()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {}", token("acc_test"))
    );
    assert_eq!(request.headers["chatgpt-account-id"], "acc_test");
    assert_eq!(request.headers["openai-beta"], transport::OPENAI_BETA_SSE);
    assert_eq!(request.headers["originator"], "pi");
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert!(!request.headers.contains_key("x-api-key"));
    let body = decode_request(request);
    assert_eq!(body["model"], "gpt-5.1-codex");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["instructions"], "You are a helpful assistant.");
    assert_eq!(body["input"][0]["content"][0]["text"], "Say hello");
    assert_eq!(body["text"], json!({"verbosity":"low"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert!(body.get("max_output_tokens").is_none());
}

/// Ports pi `openai-codex-stream.test.ts:212-331` (both terminal-body cases).
#[tokio::test]
async fn terminal_codex_events_finish_without_waiting_for_done_or_eof() {
    for (status, expected) in [
        ("completed", StopReason::Stop),
        ("incomplete", StopReason::Length),
    ] {
        let cancelled = Arc::new(AtomicBool::new(false));
        let fetch = QueueFetch::new([ResponseSpec::sse(terminal_payload(status, Some(false)))
            .stays_open()
            .tracks_cancellation(cancelled.clone())]);
        let result = stream(
            &model("gpt-5.1-codex"),
            &context("hello"),
            sse_options(fetch),
        )
        .result()
        .await
        .expect("result");
        assert_eq!(result.stop_reason, expected);
        assert_eq!(result.end_turn, Some(false));
        assert_eq!(message_text(&result), Some("Hello"));
        for _ in 0..10 {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(cancelled.load(Ordering::Acquire));
    }
}

/// Port of pi `openai-codex-stream.test.ts:333-387`.
#[tokio::test(start_paused = true)]
async fn sse_header_wait_uses_timeout_ms() {
    let fetch = Arc::new(PendingHeaderFetch::default());
    let mut options = OpenAICodexResponsesOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    options.stream.request.fetch = Some(fetch.clone());
    options.stream.request.timeout_ms = Some(50.0);
    options.stream.transport = Some(Transport::Sse);
    let started = tokio::time::Instant::now();
    let result = stream(&model("gpt-5.1-codex"), &context("hello"), options)
        .result()
        .await
        .expect("terminal result");
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_millis(50)
    );
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Codex SSE response headers timed out after 50ms")
    );
    assert!(
        fetch
            .signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
    );

    let completed = QueueFetch::new([ResponseSpec::sse(terminal_payload("completed", None))]);
    let mut completed_options = sse_options(completed.clone());
    completed_options.stream.request.timeout_ms = Some(50.0);
    stream(
        &model("gpt-5.1-codex"),
        &context("hello"),
        completed_options,
    )
    .result()
    .await
    .expect("completed response");
    let completed_signal = completed.requests()[0]
        .signal
        .clone()
        .expect("combined signal");
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(!completed_signal.is_aborted());
}

/// Pins pi `src/api/openai-codex-responses.ts:256-323,377-384`: api-key and
/// account resolution plus body construction and `onPayload` precede abort at
/// the transport request point.
#[tokio::test]
async fn preaborted_codex_preserves_key_and_payload_ordering_without_a_request() {
    let signal = Arc::new(ManualAbort::default());
    signal.abort();
    let fetch = QueueFetch::new(std::iter::empty());
    let payload_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = payload_calls.clone();
    let mut options = sse_options(fetch.clone());
    options.stream.request.signal = Some(signal.clone());
    options.stream.request.on_payload = Some(Arc::new(move |_, _| {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { None })
    }));

    let mut events = stream(&model("gpt-5.1-codex"), &context("hello"), options);
    while events.next().await.is_some() {}
    let message = events.result().await.unwrap();
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Request was aborted")
    );
    assert_eq!(payload_calls.load(Ordering::Relaxed), 1);
    assert!(fetch.requests().is_empty());

    let mut missing_key = OpenAICodexResponsesOptions::default();
    missing_key.stream.transport = Some(Transport::Sse);
    missing_key.stream.request.signal = Some(signal);
    let mut events = stream(&model("gpt-5.1-codex"), &context("hello"), missing_key);
    while events.next().await.is_some() {}
    let message = events.result().await.unwrap();
    assert_eq!(
        message.error_message.as_deref(),
        Some("No API key for provider: openai-codex")
    );
}

/// Port of pi `openai-codex-stream.test.ts:389-495`.
#[tokio::test]
async fn abort_signal_stops_sse_body_reads_after_headers() {
    let signal = Arc::new(ManualAbort::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let partial = [
        json!({
            "type":"response.output_item.added","output_index":0,
            "item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}
        }),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"one"}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    let fetch = QueueFetch::new([ResponseSpec::sse(partial)
        .stays_open()
        .tracks_cancellation(cancelled.clone())]);
    let mut options = sse_options(fetch);
    options.stream.request.signal = Some(signal.clone());
    let mut events = stream(&model("gpt-5.1-codex"), &context("hello"), options);
    let mut saw_one = false;
    while let Some(event) = events.next().await {
        if matches!(event, AssistantMessageEvent::TextDelta { ref delta, .. } if delta == "one") {
            saw_one = true;
            break;
        }
    }
    assert!(saw_one);
    signal.abort();
    let Some(AssistantMessageEvent::Error { reason, error }) = events.next().await else {
        panic!("expected terminal error");
    };
    assert_eq!(reason, ErrorStopReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(error.error_message.as_deref(), Some("Request was aborted"));
    assert!(cancelled.load(Ordering::Acquire));
}

/// Pins pi `openai-codex-responses.ts:455-461`: a successful response without
/// a body fails before the assistant start event.
#[tokio::test]
async fn missing_sse_response_body_is_an_in_band_error_before_start() {
    let mut options = OpenAICodexResponsesOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    options.stream.request.fetch = Some(Arc::new(NoBodyFetch));
    options.stream.transport = Some(Transport::Sse);
    let mut events = stream(&model("gpt-5.1-codex"), &context("hello"), options);
    let Some(AssistantMessageEvent::Error { error, .. }) = events.next().await else {
        panic!("expected terminal error without start");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("No response body"));
}

/// Pins pi `types.ts:324-336` and `openai-codex-responses.ts:230-237,476-489`:
/// neither public stream entry point synchronously fails without an async runtime.
#[test]
fn stream_entry_points_without_tokio_runtime_return_terminal_errors() {
    let current = model("gpt-5.1-codex");
    let streams = [
        stream(
            &current,
            &context("hello"),
            OpenAICodexResponsesOptions::default(),
        ),
        stream_simple(&current, &context("hello"), SimpleStreamOptions::default()),
    ];
    for mut events in streams {
        let event = futures::executor::block_on(events.next()).expect("terminal event");
        assert!(matches!(event, AssistantMessageEvent::Error { .. }));
        let message = futures::executor::block_on(events.result()).expect("terminal result");
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("Tokio runtime is not available")
        );
    }
}

/// Ports pi `openai-codex-stream.test.ts:497-745,1131-1226`.
#[tokio::test]
async fn sse_session_affinity_presence_and_clamping_match_pi() {
    for (session, retention, expected_header, expected_body) in [
        (
            Some("test-session-123".to_owned()),
            None,
            Some("test-session-123".to_owned()),
            Some("test-session-123".to_owned()),
        ),
        (
            Some("one-off-summary".to_owned()),
            Some(CacheRetention::None),
            None,
            None,
        ),
        (
            Some("x".repeat(67)),
            None,
            Some("x".repeat(64)),
            Some("x".repeat(64)),
        ),
        (Some(String::new()), None, None, Some(String::new())),
        (None, None, None, None),
    ] {
        let fetch = QueueFetch::new([ResponseSpec::sse(terminal_payload("completed", None))]);
        let mut options = sse_options(fetch.clone());
        options.stream.session_id = session;
        options.stream.cache_retention = retention;
        stream(&model("gpt-5.1-codex"), &context("hello"), options)
            .result()
            .await
            .expect("result");
        let request = &fetch.requests()[0];
        assert_eq!(request.headers.get("session-id"), expected_header.as_ref());
        assert_eq!(
            request.headers.get("x-client-request-id"),
            expected_header.as_ref()
        );
        assert!(!request.headers.contains_key("session_id"));
        assert_eq!(
            decode_request(request)
                .get("prompt_cache_key")
                .and_then(Value::as_str),
            expected_body.as_deref()
        );
    }
}

/// Ports pi `openai-codex-stream.test.ts:747-861,932-1030`; explicit-null and
/// whole-number formatting pin pi `openai-codex-responses.ts:283,548-576`.
#[test]
fn reasoning_and_tool_choice_request_lowering_matches_pi() {
    let mut gpt55 = model("gpt-5.5");
    gpt55.thinking_level_map = Some(crate::types::ThinkingLevelMap {
        xhigh: Some(Some("xhigh".to_owned())),
        minimal: Some(Some("low".to_owned())),
        ..Default::default()
    });
    let grammar = BTreeMap::new();
    let mut options = OpenAICodexResponsesOptions {
        reasoning_effort: Some(CodexReasoningEffort::Xhigh),
        tool_choice: Some(ResponseToolChoiceMode::Required),
        ..Default::default()
    };
    let body =
        build_request_body(&gpt55, &context("hello"), &options, None, &grammar).expect("body");
    assert_eq!(
        body["reasoning"],
        json!({"effort":"xhigh","summary":"auto"})
    );
    assert_eq!(body["tool_choice"], "required");
    for id in ["gpt-5.3-codex", "gpt-5.4", "gpt-5.5"] {
        let mut current = model(id);
        current.thinking_level_map = gpt55.thinking_level_map.clone();
        options.reasoning_effort = Some(CodexReasoningEffort::Minimal);
        let body = build_request_body(&current, &context("hello"), &options, None, &grammar)
            .expect("body");
        assert_eq!(body["reasoning"], json!({"effort":"low","summary":"auto"}));
    }
    let mut null_mapping = model("gpt-5.5");
    null_mapping.thinking_level_map = Some(crate::types::ThinkingLevelMap {
        minimal: Some(None),
        ..Default::default()
    });
    let body = build_request_body(
        &null_mapping,
        &context("hello"),
        &OpenAICodexResponsesOptions {
            reasoning_effort: Some(CodexReasoningEffort::Minimal),
            ..Default::default()
        },
        None,
        &grammar,
    )
    .expect("body");
    assert_eq!(
        body["reasoning"],
        json!({"effort":"minimal","summary":"auto"})
    );
    let body = build_request_body(
        &gpt55,
        &context("hello"),
        &OpenAICodexResponsesOptions {
            stream: StreamOptions {
                temperature: Some(1.0),
                ..Default::default()
            },
            reasoning_effort: Some(CodexReasoningEffort::Low),
            reasoning_summary: Some(None),
            service_tier: Some(None),
            ..Default::default()
        },
        None,
        &grammar,
    )
    .expect("presence body");
    assert_eq!(body["temperature"], 1);
    let wire = serde_json::to_string(&body).unwrap();
    assert!(wire.contains(r#""temperature":1"#));
    assert!(!wire.contains(r#""temperature":1.0"#));
    assert_eq!(body["service_tier"], Value::Null);
    assert_eq!(body["reasoning"]["summary"], "auto");
}

/// Pins pi `openai-codex-responses.ts:638-651,1050-1059`: URL derivation and
/// the UUIDv7 correlation id used when cache affinity is absent.
#[test]
fn codex_url_and_uncached_websocket_request_id_match_pi() {
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_websocket_url("https://chatgpt.com/backend-api").expect("URL"),
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    let request_id = websocket_request_id(None);
    let parsed = uuid::Uuid::parse_str(&request_id).expect("UUID");
    assert_eq!(parsed.get_version_num(), 7);
    assert!(uuid::Uuid::parse_str(&websocket_request_id(Some(""))).is_ok());
    assert_eq!(websocket_request_id(Some("session")), "session");
}

/// Ports pi `openai-codex-stream.test.ts:747-861` through the public stream entry points.
#[tokio::test]
async fn public_streams_forward_xhigh_reasoning_and_required_tool_choice() {
    let fetch = QueueFetch::new([
        ResponseSpec::sse(terminal_payload("completed", None)),
        ResponseSpec::sse(terminal_payload("completed", None)),
    ]);
    let mut current = model("gpt-5.5");
    current.thinking_level_map = Some(crate::types::ThinkingLevelMap {
        xhigh: Some(Some("xhigh".to_owned())),
        ..Default::default()
    });
    let mut simple = SimpleStreamOptions::default();
    simple.stream.request.api_key = Some(token("acc_test"));
    simple.stream.request.fetch = Some(fetch.clone());
    simple.stream.transport = Some(Transport::Sse);
    simple.reasoning = Some(ThinkingLevel::Xhigh);
    stream_simple(&current, &context("hello"), simple)
        .result()
        .await
        .expect("simple result");

    let mut direct = sse_options(fetch.clone());
    direct.tool_choice = Some(ResponseToolChoiceMode::Required);
    stream(&current, &context("hello"), direct)
        .result()
        .await
        .expect("direct result");

    let requests = fetch.requests();
    assert_eq!(
        decode_request(&requests[0])["reasoning"],
        json!({"effort":"xhigh","summary":"auto"})
    );
    assert_eq!(decode_request(&requests[1])["tool_choice"], "required");
}

/// Port of pi `openai-codex-stream.test.ts:863-930`.
#[test]
fn codex_strict_mode_and_constrained_sampling_match_pi() {
    let mut current = model("gpt-5.5");
    current.compat = Some(ModelCompat::OpenAIResponses(OpenAIResponsesCompat {
        supports_open_ai_grammar_tools: Some(true),
        ..Default::default()
    }));
    let mut current_context = context("Use a tool");
    current_context.tools = Some(vec![
        Tool {
            name: "optional".to_owned(),
            description: "Optional".to_owned(),
            parameters: json!({"type":"object","properties":{"value":{"type":"string"}}}),
            constrained_sampling: Some(ToolConstrainedSampling::Disabled),
        },
        Tool {
            name: "strict".to_owned(),
            description: "Strict".to_owned(),
            parameters: json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false}),
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::JsonSchema {
                    strict: StrictPreference::Prefer,
                },
            )),
        },
        Tool {
            name: "grammar".to_owned(),
            description: "Grammar".to_owned(),
            parameters: json!({
                "type":"object",
                "properties":{"payload":{"type":"string"}},
                "required":["payload"]
            }),
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::Grammar {
                    variants: GrammarVariants {
                        openai_lark: Some("start: /[a-z]+/".to_owned()),
                        openai_regex: None,
                    },
                },
            )),
        },
    ]);
    let grammar = create_grammar_tool_input_properties(current_context.tools.as_deref(), true)
        .expect("grammar properties");
    let body = build_request_body(
        &current,
        &current_context,
        &OpenAICodexResponsesOptions::default(),
        None,
        &grammar,
    )
    .expect("body");
    assert_eq!(body["tools"][0]["strict"], Value::Null);
    assert_eq!(body["tools"][1]["strict"], true);
    assert_eq!(body["tools"][2]["type"], "custom");
}

/// Ports pi `openai-codex-stream.test.ts:1032-1129`.
#[tokio::test]
async fn client_service_tier_prices_codex_default_echo() {
    for (id, tier, multiplier) in [
        ("gpt-5.1-codex", ResponseServiceTier::Flex, 0.5),
        ("gpt-5.1-codex", ResponseServiceTier::Priority, 2.0),
        ("gpt-5.5", ResponseServiceTier::Flex, 0.5),
        ("gpt-5.5", ResponseServiceTier::Priority, 2.5),
    ] {
        let terminal = [
            json!({"type":"response.completed","response":{"status":"completed","service_tier":"default","usage":{"input_tokens":1_000_000,"output_tokens":1_000_000,"total_tokens":2_000_000}}})
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let fetch = QueueFetch::new([ResponseSpec::sse(terminal)]);
        let mut options = sse_options(fetch);
        options.service_tier = Some(Some(tier));
        let mut current = model(id);
        current.cost = ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                ..Default::default()
            },
            tiers: None,
        };
        let result = stream(&current, &context("hello"), options)
            .result()
            .await
            .expect("result");
        assert_eq!(result.usage.cost.input, 1.0 * multiplier);
        assert_eq!(result.usage.cost.output, 2.0 * multiplier);
        assert_eq!(result.usage.cost.total, 3.0 * multiplier);
    }
}

/// Pins pi `src/api/openai-codex-responses.ts:599-630`: an unknown echoed
/// service tier passes through resolution and uses multiplier one.
#[test]
fn unknown_service_tier_passes_through_resolution_at_multiplier_one() {
    let tier = ResponseServiceTier::Other("future-tier".to_owned());
    assert_eq!(
        resolve_codex_service_tier(
            Some(Some(tier.clone())),
            Some(Some(ResponseServiceTier::Priority)),
        ),
        Some(Some(tier.clone()))
    );
    assert_eq!(service_tier_multiplier(&model("gpt-5.5"), Some(tier)), 1.0);
}

/// Ports pi `openai-codex-stream.test.ts:2360-2432` (all retry header forms).
#[tokio::test(start_paused = true)]
async fn sse_retries_honor_retry_after_ms_seconds_and_http_date() {
    let now = SystemTime::now();
    let http_date = httpdate::fmt_http_date(now + Duration::from_secs(45));
    for (name, headers, expected) in [
        (
            "retry-after-ms",
            BTreeMap::from([("retry-after-ms".to_owned(), "1500".to_owned())]),
            Duration::from_millis(1_500),
        ),
        (
            "retry-after seconds",
            BTreeMap::from([("retry-after".to_owned(), "60".to_owned())]),
            Duration::from_secs(60),
        ),
        (
            "retry-after HTTP date",
            BTreeMap::from([("retry-after".to_owned(), http_date.clone())]),
            Duration::from_secs(45),
        ),
    ] {
        let fetch = QueueFetch::new([
            ResponseSpec::error(
                429,
                r#"{"error":{"code":"rate_limit_exceeded","message":"rate limited"}}"#,
            )
            .with_headers(headers),
            ResponseSpec::sse(terminal_payload("completed", None)),
        ]);
        let mut options = sse_options(fetch.clone());
        options.stream.request.max_retries = Some(1.0);
        let started = tokio::time::Instant::now();
        let result = stream(&model("gpt-5.1-codex"), &context("hello"), options)
            .result()
            .await
            .expect("result");
        let elapsed = tokio::time::Instant::now() - started;
        if name == "retry-after HTTP date" {
            assert!(
                (Duration::from_secs(43)..=expected).contains(&elapsed),
                "{name}: {elapsed:?}"
            );
        } else {
            assert_eq!(elapsed, expected, "{name}");
        }
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(fetch.calls.load(Ordering::Relaxed), 2);
    }
}

/// Ports pi `openai-codex-stream.test.ts:2434-2472` (429 and 503 rows).
#[tokio::test]
async fn sse_retry_delay_over_limit_fails_without_another_request() {
    for status in [429, 503] {
        let fetch = QueueFetch::new([ResponseSpec::error(
            status,
            r#"{"error":{"code":"temporarily_unavailable","message":"retry later"}}"#,
        )
        .with_headers([("retry-after".to_owned(), "2".to_owned())])]);
        let mut options = sse_options(fetch.clone());
        options.stream.request.max_retries = Some(3.0);
        options.stream.request.max_retry_delay_ms = Some(1_000.0);
        let result = stream(&model("gpt-5.1-codex"), &context("hello"), options)
            .result()
            .await
            .expect("terminal result");
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Server requested 2s retry delay (max: 1s)")
        );
        assert_eq!(fetch.calls.load(Ordering::Relaxed), 1);
    }
}

/// Port of pi `openai-codex-stream.test.ts:2547-2621`.
#[tokio::test(start_paused = true)]
async fn sse_retries_use_exponential_backoff_without_retry_headers() {
    let error = || {
        ResponseSpec::error(
            429,
            r#"{"error":{"code":"rate_limit_exceeded","message":"rate limited"}}"#,
        )
    };
    let fetch = QueueFetch::new([
        error(),
        error(),
        error(),
        ResponseSpec::sse(terminal_payload("completed", None)),
    ]);
    let mut options = sse_options(fetch.clone());
    options.stream.request.max_retries = Some(3.0);
    let started = tokio::time::Instant::now();
    let result = stream(&model("gpt-5.1-codex"), &context("hello"), options)
        .result()
        .await
        .expect("result");
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(7)
    );
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(fetch.calls.load(Ordering::Relaxed), 4);
}

/// Pins pi `openai-codex-responses.ts:38,374-448`: zero retries by default and
/// text-classified transient responses participate in the same local retry loop.
#[tokio::test(start_paused = true)]
async fn sse_retry_default_and_text_classification_match_pi() {
    let default_fetch = QueueFetch::new([
        ResponseSpec::error(503, "unavailable"),
        ResponseSpec::sse(terminal_payload("completed", None)),
    ]);
    let result = stream(
        &model("gpt-5.1-codex"),
        &context("hello"),
        sse_options(default_fetch.clone()),
    )
    .result()
    .await
    .expect("terminal result");
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(default_fetch.calls.load(Ordering::Relaxed), 1);

    let classified = QueueFetch::new([
        ResponseSpec::error(400, "upstream connection refused"),
        ResponseSpec::sse(terminal_payload("completed", None)),
    ]);
    let mut options = sse_options(classified.clone());
    options.stream.request.max_retries = Some(1.0);
    let result = stream(&model("gpt-5.1-codex"), &context("hello"), options)
        .result()
        .await
        .expect("result");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(classified.calls.load(Ordering::Relaxed), 2);

    let network = NetworkThenFetch::new(ResponseSpec::sse(terminal_payload("completed", None)));
    let mut network_options = OpenAICodexResponsesOptions::default();
    network_options.stream.request.api_key = Some(token("acc_test"));
    network_options.stream.request.fetch = Some(network.clone());
    network_options.stream.request.max_retries = Some(1.0);
    network_options.stream.transport = Some(Transport::Sse);
    let started = tokio::time::Instant::now();
    let result = stream(&model("gpt-5.1-codex"), &context("hello"), network_options)
        .result()
        .await
        .expect("network retry result");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(network.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(1)
    );
}

/// Pins pi `openai-codex-responses.ts:129`: JavaScript `.?` consumes at most
/// one non-line-terminator UTF-16 code unit.
#[test]
fn retryable_pattern_optional_character_excludes_js_line_terminators() {
    for separator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
        assert!(!is_retryable_error(400, &format!("rate{separator}limit")));
    }
    assert!(!is_retryable_error(400, "rate😀limit"));
    assert!(is_retryable_error(400, "rate limit"));
    assert!(is_retryable_error(400, "rateélimit"));
    assert!(is_retryable_error(400, "ratelimit"));
}

/// Pins pi `openai-codex-responses.ts:428-433,1551`: an empty HTTP/2 reason
/// phrase remains empty so an empty non-2xx body reports `Request failed`.
#[test]
fn empty_http2_error_uses_request_failed_without_a_canonical_reason() {
    let response: reqwest::Response = http::Response::builder()
        .version(http::Version::HTTP_2)
        .status(502)
        .body(Vec::<u8>::new())
        .expect("response")
        .into();
    let status_text = reqwest_status_text(&response);
    assert!(status_text.is_empty());
    assert_eq!(
        parse_error_response(502, &status_text, ""),
        "Request failed"
    );
}

/// Port of pi `openai-codex-stream.test.ts:2474-2545`.
#[tokio::test]
async fn zstd_compresses_every_sse_request_body_at_level_three() {
    for text in ["compress me ".repeat(400), "hi".to_owned()] {
        let fetch = QueueFetch::new([ResponseSpec::sse(terminal_payload("completed", None))]);
        stream(
            &model("gpt-5.1-codex"),
            &context(&text),
            sse_options(fetch.clone()),
        )
        .result()
        .await
        .expect("result");
        let request = &fetch.requests()[0];
        assert_eq!(
            request.headers.get("content-encoding").map(String::as_str),
            Some("zstd")
        );
        assert_eq!(
            decode_request(request)["input"][0]["content"][0]["text"],
            text
        );
    }
}

fn message_text(message: &AssistantMessage) -> Option<&str> {
    message.content.iter().find_map(|content| match content {
        AssistantContent::Text(text) => Some(text.text.as_str()),
        AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
    })
}

#[derive(Clone)]
enum WsAction {
    Events(Vec<Value>),
    Close,
    CloseWithCode { code: u16, reason: String },
    Idle,
}

#[derive(Default)]
struct LocalServerState {
    actions: Mutex<VecDeque<WsAction>>,
    websocket_requests: Mutex<Vec<Value>>,
    websocket_headers: Mutex<Vec<BTreeMap<String, String>>>,
    websocket_connections: AtomicUsize,
    websocket_disconnections: AtomicUsize,
    websocket_handshake_stalls: AtomicUsize,
    http_requests: AtomicUsize,
    shutdown: Notify,
}

struct LocalCodexServer {
    address: SocketAddr,
    state: Arc<LocalServerState>,
    task: JoinHandle<()>,
}

impl LocalCodexServer {
    async fn start(actions: Vec<WsAction>, sse_payload: String) -> Self {
        Self::start_with_handshake_stalls(actions, sse_payload, 0).await
    }

    async fn start_with_handshake_stalls(
        actions: Vec<WsAction>,
        sse_payload: String,
        handshake_stalls: usize,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(LocalServerState {
            actions: Mutex::new(actions.into()),
            websocket_handshake_stalls: AtomicUsize::new(handshake_stalls),
            ..LocalServerState::default()
        });
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let state = server_state.clone();
                let payload = sse_payload.clone();
                tokio::spawn(async move {
                    if is_websocket_upgrade(&socket).await {
                        let stall = state
                            .websocket_handshake_stalls
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                                remaining.checked_sub(1)
                            })
                            .is_ok();
                        if stall {
                            state.websocket_connections.fetch_add(1, Ordering::Relaxed);
                            state.shutdown.notified().await;
                        } else {
                            serve_websocket(socket, state).await;
                        }
                    } else {
                        serve_http(socket, state, payload).await;
                    }
                });
            }
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/backend-api", self.address)
    }

    fn requests(&self) -> Vec<Value> {
        self.state
            .websocket_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn headers(&self) -> Vec<BTreeMap<String, String>> {
        self.state
            .websocket_headers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for LocalCodexServer {
    fn drop(&mut self) {
        self.state.shutdown.notify_waiters();
        self.task.abort();
    }
}

async fn is_websocket_upgrade(socket: &TcpStream) -> bool {
    let mut bytes = [0_u8; 8_192];
    for _ in 0..20 {
        let count = socket.peek(&mut bytes).await.unwrap_or_default();
        let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
        if request.contains("\r\n\r\n") {
            return request.contains("upgrade: websocket");
        }
        tokio::task::yield_now().await;
    }
    false
}

#[allow(clippy::result_large_err)]
async fn serve_websocket(socket: TcpStream, state: Arc<LocalServerState>) {
    state.websocket_connections.fetch_add(1, Ordering::Relaxed);
    let captured = state.clone();
    let accepted = accept_hdr_async(socket, move |request: &Request, response: Response| {
        let headers = request
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        captured
            .websocket_headers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(headers);
        Ok(response)
    })
    .await;
    let Ok(mut websocket) = accepted else {
        state
            .websocket_disconnections
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
    while let Some(Ok(message)) = websocket.next().await {
        let WsMessage::Text(text) = message else {
            if matches!(message, WsMessage::Close(_)) {
                state
                    .websocket_disconnections
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            continue;
        };
        let request = serde_json::from_str::<Value>(&text).expect("response.create JSON");
        state
            .websocket_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let action = state
            .actions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or(WsAction::Idle);
        match action {
            WsAction::Events(events) => {
                for event in events {
                    if websocket
                        .send(WsMessage::Text(event.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            WsAction::Close => {
                let _ = websocket.close(None).await;
                state
                    .websocket_disconnections
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            WsAction::CloseWithCode { code, reason } => {
                let _ = websocket
                    .send(WsMessage::Close(Some(
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: code.into(),
                            reason: reason.into(),
                        },
                    )))
                    .await;
                state
                    .websocket_disconnections
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            WsAction::Idle => {}
        }
    }
    state
        .websocket_disconnections
        .fetch_add(1, Ordering::Relaxed);
}

async fn serve_http(socket: TcpStream, state: Arc<LocalServerState>, payload: String) {
    let service = service_fn(move |_request| {
        state.http_requests.fetch_add(1, Ordering::Relaxed);
        let payload = payload.clone();
        async move {
            Ok::<_, Infallible>(
                hyper::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Full::new(Bytes::from(payload)))
                    .expect("response"),
            )
        }
    });
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(socket), service)
        .await;
}

fn response_events(response_id: &str, message_id: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":response_id}}),
        json!({
            "type":"response.output_item.added","output_index":0,
            "item":{"type":"message","id":message_id,"role":"assistant","status":"in_progress","content":[]}
        }),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({
            "type":"response.output_item.done","output_index":0,
            "item":{"type":"message","id":message_id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}
        }),
        json!({
            "type":"response.completed",
            "response":{"id":response_id,"status":"completed","end_turn":false,"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}
        }),
    ]
}

fn websocket_options(
    session_id: Option<&str>,
    transport: Transport,
) -> OpenAICodexResponsesOptions {
    let mut options = OpenAICodexResponsesOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    options.stream.transport = Some(transport);
    options.stream.session_id = session_id.map(str::to_owned);
    options
}

async fn collect_event_kinds(
    mut events: AssistantMessageEventStream,
) -> (Vec<&'static str>, AssistantMessage) {
    let result = events.result_handle();
    let mut kinds = Vec::new();
    while let Some(event) = events.next().await {
        kinds.push(match event {
            AssistantMessageEvent::Start => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        });
    }
    (kinds, result.get().await.expect("terminal result"))
}

/// Required WS pin from pi `openai-codex-responses.ts:711-754,1456-1511`:
/// response.create shape and event normalization parity with SSE.
#[tokio::test]
async fn websocket_response_create_shape_and_normalization_match_sse() {
    let server = LocalCodexServer::start(
        vec![WsAction::Events(response_events(
            "resp_ws", "msg_ws", "Hello",
        ))],
        terminal_payload("completed", Some(false)),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let (websocket_kinds, websocket_result) = collect_event_kinds(stream(
        &current,
        &context("Say hello"),
        websocket_options(Some("session-frame"), Transport::Websocket),
    ))
    .await;
    assert_eq!(message_text(&websocket_result), Some("Hello"));
    assert_eq!(websocket_result.end_turn, Some(false));
    let (sse_kinds, sse_result) = collect_event_kinds(stream(
        &current,
        &context("Say hello"),
        websocket_options(None, Transport::Sse),
    ))
    .await;
    assert_eq!(websocket_kinds, sse_kinds);
    assert_eq!(message_text(&sse_result), message_text(&websocket_result));
    assert_eq!(sse_result.end_turn, websocket_result.end_turn);
    let request = &server.requests()[0];
    assert_eq!(request["type"], "response.create");
    assert_eq!(request["model"], "gpt-5.1-codex");
    assert_eq!(request["store"], false);
    assert_eq!(request["stream"], true);
    assert_eq!(request["input"][0]["content"][0]["text"], "Say hello");
    let headers = &server.headers()[0];
    assert_eq!(headers["openai-beta"], transport::OPENAI_BETA_WEBSOCKETS);
    assert_eq!(headers["session-id"], "session-frame");
    assert_eq!(headers["x-client-request-id"], "session-frame");
    assert_eq!(headers["chatgpt-account-id"], "acc_test");
    close_open_ai_codex_websocket_sessions(Some("session-frame"));
}

/// Required WS pin from pi `openai-codex-responses.ts:318-401,930-955`:
/// pre-stream fallback plus per-session fallback memory. The numeric close-code
/// assertion pins `utils/diagnostics.ts:21-30`.
#[tokio::test]
async fn websocket_pre_stream_failure_falls_back_and_is_remembered() {
    let server = LocalCodexServer::start(
        vec![WsAction::CloseWithCode {
            code: 1009,
            reason: String::new(),
        }],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    for attempt in 0..2 {
        let result = stream(
            &current,
            &context("Say hello"),
            websocket_options(Some("session-fallback"), Transport::Auto),
        )
        .result()
        .await
        .expect("fallback result");
        assert_eq!(message_text(&result), Some("Hello"));
        if attempt == 0 {
            let diagnostic = result
                .diagnostics
                .as_deref()
                .and_then(|diagnostics| diagnostics.first())
                .expect("fallback diagnostic");
            let error = diagnostic.error.as_ref().expect("diagnostic error");
            assert_eq!(error.name.as_deref(), Some("WebSocketCloseError"));
            assert_eq!(error.message, "WebSocket closed 1009 message too big");
            assert_eq!(
                error.code,
                Some(DiagnosticCode::Number(serde_json::Number::from(1009)))
            );
        }
    }
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        1
    );
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 2);
    let stats = get_open_ai_codex_websocket_debug_stats("session-fallback").expect("stats");
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 2);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    reset_open_ai_codex_websocket_debug_stats(Some("session-fallback"));
}

/// Pins pi `openai-codex-responses.ts:325,348-363`: semantic stop-reason errors
/// after clean WebSocket completion use the transport-failure path and pin SSE.
#[tokio::test]
async fn websocket_semantic_terminal_errors_record_failure_and_pin_sse() {
    for (index, status, incomplete_reason, expected_error) in [
        (
            0,
            "incomplete",
            Some("content_filter"),
            "Response incomplete: content_filter",
        ),
        (1, "failed", None, "An unknown error occurred"),
        (2, "cancelled", None, "An unknown error occurred"),
    ] {
        let terminal_type = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let server = LocalCodexServer::start(
            vec![WsAction::Events(vec![json!({
                "type":terminal_type,
                "response":{
                    "id":format!("resp_semantic_{index}"),
                    "status":status,
                    "incomplete_details":incomplete_reason.map(|reason| json!({"reason":reason})),
                    "usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}
                }
            })])],
            terminal_payload("completed", None),
        )
        .await;
        let mut current = model("gpt-5.1-codex");
        current.base_url = server.base_url();
        let session_id = format!("semantic-status-{index}");
        let (kinds, result) = collect_event_kinds(stream(
            &current,
            &context("hello"),
            websocket_options(Some(&session_id), Transport::Auto),
        ))
        .await;
        assert_eq!(kinds, ["start", "error"]);
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some(expected_error));
        let diagnostic = result
            .diagnostics
            .as_deref()
            .and_then(|diagnostics| diagnostics.first())
            .expect("transport diagnostic");
        assert_eq!(diagnostic.kind, "provider_transport_failure");
        assert_eq!(
            diagnostic
                .details
                .as_ref()
                .and_then(|details| details.get("eventsEmitted")),
            Some(&Value::Bool(true))
        );
        assert!(
            diagnostic
                .details
                .as_ref()
                .is_some_and(|details| !details.contains_key("fallbackTransport"))
        );

        let recovered = stream(
            &current,
            &context("hello again"),
            websocket_options(Some(&session_id), Transport::Auto),
        )
        .result()
        .await
        .expect("SSE recovery result");
        assert_eq!(recovered.stop_reason, StopReason::Stop);
        assert_eq!(
            server.state.websocket_connections.load(Ordering::Relaxed),
            1
        );
        assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 1);
        let stats = get_open_ai_codex_websocket_debug_stats(&session_id).expect("stats");
        assert_eq!(stats.websocket_failures, 1);
        assert_eq!(stats.sse_fallbacks, 1);
        assert_eq!(stats.websocket_fallback_active, Some(true));
        assert_eq!(stats.last_websocket_error.as_deref(), Some(expected_error));
        close_open_ai_codex_websocket_sessions(Some(&session_id));
        reset_open_ai_codex_websocket_debug_stats(Some(&session_id));
    }
}

/// Pins pi `openai-responses-shared.ts:485-502`: the Codex producer exposes a
/// function-call namespace in its start-time partial state.
#[tokio::test]
async fn codex_toolcall_start_carries_start_time_namespace() {
    let events = [
        json!({
            "type":"response.output_item.added","output_index":0,
            "item":{
                "type":"function_call","id":"fc_1","call_id":"call_1",
                "name":"lookup","namespace":"dynamic_tools","arguments":"{}"
            }
        }),
        json!({
            "type":"response.output_item.done","output_index":0,
            "item":{
                "type":"function_call","id":"fc_1","call_id":"call_1",
                "name":"lookup","namespace":"dynamic_tools","arguments":"{}"
            }
        }),
        json!({
            "type":"response.completed",
            "response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}
        }),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    let fetch = QueueFetch::new([ResponseSpec::sse(events)]);
    let mut stream = stream(
        &model("gpt-5.1-codex"),
        &context("hello"),
        sse_options(fetch),
    );
    let mut namespace = None;
    while let Some(event) = stream.next().await {
        if let AssistantMessageEvent::ToolCallStart {
            namespace: event_namespace,
            ..
        } = event
        {
            namespace = event_namespace;
        }
    }
    assert_eq!(namespace.as_deref(), Some("dynamic_tools"));
}

/// Required WS pin from pi `openai-codex-responses.ts:1107-1208,1388-1454`:
/// cache reuse and previous_response_id input-delta continuation.
#[tokio::test]
async fn websocket_cache_reuses_session_socket_and_sends_only_input_delta() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
            WsAction::Events(response_events("resp_2", "msg_2", "Recovered")),
        ],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let first_context = context("Say hello");
    let first = stream(
        &current,
        &first_context,
        websocket_options(Some("session-cache"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("first");
    let mut second_context = first_context.clone();
    second_context
        .messages
        .push(Message::Assistant(Box::new(first)));
    second_context
        .messages
        .push(Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("Now finish".to_owned()),
            timestamp: 2,
        })));
    let second = stream(
        &current,
        &second_context,
        websocket_options(Some("session-cache"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("second");
    assert_eq!(message_text(&second), Some("Recovered"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("previous_response_id").is_none());
    assert_eq!(requests[1]["previous_response_id"], "resp_1");
    assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(requests[1]["input"][0]["content"][0]["text"], "Now finish");
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        1
    );
    let stats = get_open_ai_codex_websocket_debug_stats("session-cache").expect("stats");
    assert_eq!(stats.connections_created, 1);
    assert_eq!(stats.connections_reused, 1);
    assert_eq!(stats.full_context_requests, 1);
    assert_eq!(stats.delta_requests, 1);
    close_open_ai_codex_websocket_sessions(Some("session-cache"));
    reset_open_ai_codex_websocket_debug_stats(Some("session-cache"));
}

/// Pins pi `openai-codex-responses.ts:1328-1383`: auto uses continuation deltas,
/// while plain websocket reuses the socket but continues to send full context.
#[tokio::test]
async fn auto_uses_input_delta_while_plain_websocket_uses_full_context() {
    for (transport, expects_delta) in [(Transport::Auto, true), (Transport::Websocket, false)] {
        let session_id = if expects_delta {
            "auto-delta-session"
        } else {
            "plain-websocket-session"
        };
        let server = LocalCodexServer::start(
            vec![
                WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
                WsAction::Events(response_events("resp_2", "msg_2", "Again")),
            ],
            "unexpected SSE".to_owned(),
        )
        .await;
        let mut current = model("gpt-5.1-codex");
        current.base_url = server.base_url();
        let first_context = context("hello");
        let first = stream(
            &current,
            &first_context,
            websocket_options(Some(session_id), transport),
        )
        .result()
        .await
        .expect("first");
        let mut next = first_context.clone();
        next.messages.extend([
            Message::Assistant(Box::new(first)),
            Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("again".to_owned()),
                timestamp: 2,
            })),
        ]);
        stream(
            &current,
            &next,
            websocket_options(Some(session_id), transport),
        )
        .result()
        .await
        .expect("second");
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            server.state.websocket_connections.load(Ordering::Relaxed),
            1
        );
        if expects_delta {
            assert_eq!(requests[1]["previous_response_id"], "resp_1");
            assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(1));
        } else {
            assert!(requests[1].get("previous_response_id").is_none());
            assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
        }
        close_open_ai_codex_websocket_sessions(Some(session_id));
        reset_open_ai_codex_websocket_debug_stats(Some(session_id));
    }
}

/// Port of pi `openai-codex-stream.test.ts:1227-1342`.
#[tokio::test]
async fn stream_simple_forwards_auto_and_uses_cached_websocket_context() {
    let server = LocalCodexServer::start(
        vec![WsAction::Events(response_events(
            "resp_1", "msg_1", "Hello",
        ))],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let mut options = SimpleStreamOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    options.stream.session_id = Some("session-auto".to_owned());
    options.stream.transport = Some(Transport::Auto);
    let result = stream_simple(&current, &context("Say hello"), options)
        .result()
        .await
        .expect("result");
    assert_eq!(result.end_turn, Some(false));
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 0);
    let headers = &server.headers()[0];
    assert_eq!(headers["session-id"], "session-auto");
    assert_eq!(headers["x-client-request-id"], "session-auto");
    assert!(!headers.contains_key("session_id"));
    let stats = get_open_ai_codex_websocket_debug_stats("session-auto").expect("stats");
    assert_eq!(stats.cached_context_requests, 1);
    assert_eq!(stats.full_context_requests, 1);
    close_open_ai_codex_websocket_sessions(Some("session-auto"));
    reset_open_ai_codex_websocket_debug_stats(Some("session-auto"));
}

/// Port of pi `openai-codex-stream.test.ts:1344-1433`.
#[tokio::test]
async fn cached_websockets_are_scoped_to_the_authenticated_account() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "A")),
            WsAction::Events(response_events("resp_2", "msg_2", "B")),
            WsAction::Events(response_events("resp_3", "msg_3", "A2")),
        ],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    for account in ["account-a", "account-b", "account-a"] {
        let mut options = websocket_options(Some("shared-session"), Transport::WebsocketCached);
        options.stream.request.api_key = Some(token(account));
        stream(&current, &Context::default(), options)
            .result()
            .await
            .expect("result");
    }
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    assert_eq!(
        server
            .headers()
            .iter()
            .map(|headers| headers["chatgpt-account-id"].as_str())
            .collect::<Vec<_>>(),
        ["account-a", "account-b"]
    );
    let stats = get_open_ai_codex_websocket_debug_stats("shared-session").expect("stats");
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 1);
    close_open_ai_codex_websocket_sessions(Some("shared-session"));
    reset_open_ai_codex_websocket_debug_stats(Some("shared-session"));
}

/// Port of pi `openai-codex-stream.test.ts:1435-1527`.
#[tokio::test]
async fn cache_retention_none_uses_one_shot_websockets_without_cache_affinity() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "One")),
            WsAction::Events(response_events("resp_2", "msg_2", "Two")),
        ],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    for _ in 0..2 {
        let mut options = websocket_options(Some("one-off-summary"), Transport::Auto);
        options.stream.cache_retention = Some(CacheRetention::None);
        stream(&current, &context("hello"), options)
            .result()
            .await
            .expect("result");
    }
    for _ in 0..50 {
        if server
            .state
            .websocket_disconnections
            .load(Ordering::Relaxed)
            == 2
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    assert_eq!(
        server
            .state
            .websocket_disconnections
            .load(Ordering::Relaxed),
        2
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.get("prompt_cache_key").is_none())
    );
    assert!(get_open_ai_codex_websocket_debug_stats("one-off-summary").is_none());
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 0);
}

/// Port of pi `openai-codex-stream.test.ts:1529-1614`.
#[tokio::test]
async fn websocket_connect_timeout_falls_back_to_sse() {
    let server = LocalCodexServer::start_with_handshake_stalls(
        Vec::new(),
        terminal_payload("completed", None),
        1,
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let mut options = websocket_options(Some("ws-connect-timeout"), Transport::Auto);
    options.stream.request.timeout_ms = Some(300_000.0);
    options.stream.websocket_connect_timeout_ms = Some(25.0);
    let result = stream(&current, &context("hello"), options)
        .result()
        .await
        .expect("fallback result");
    assert_eq!(message_text(&result), Some("Hello"));
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 1);
    let stats = get_open_ai_codex_websocket_debug_stats("ws-connect-timeout").expect("stats");
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    assert_eq!(
        stats.last_websocket_error.as_deref(),
        Some("WebSocket connect timeout after 25ms")
    );
    reset_open_ai_codex_websocket_debug_stats(Some("ws-connect-timeout"));
}

/// Port of pi `openai-codex-stream.test.ts:1616-1676`.
#[tokio::test]
async fn websocket_connection_limit_gets_one_immediate_retry() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(vec![json!({
                "type":"error",
                "error":{"code":WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE}
            })]),
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
        ],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let mut options = OpenAICodexResponsesOptions::default();
    options.stream.request.api_key = Some(token("acc_test"));
    let result = stream(&current, &context("hello"), options)
        .result()
        .await
        .expect("result");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 0);
}

/// Port of pi `openai-codex-stream.test.ts:1678-1778`.
#[tokio::test]
async fn websocket_idle_before_first_event_falls_back_to_sse() {
    let server =
        LocalCodexServer::start(vec![WsAction::Idle], terminal_payload("completed", None)).await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let mut options = websocket_options(Some("ws-idle-before-start"), Transport::Auto);
    options.stream.request.timeout_ms = Some(25.0);
    let result = stream(&current, &context("hello"), options)
        .result()
        .await
        .expect("fallback result");
    assert_eq!(message_text(&result), Some("Hello"));
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 1);
    let stats = get_open_ai_codex_websocket_debug_stats("ws-idle-before-start").expect("stats");
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    assert_eq!(stats.websocket_fallback_active, Some(true));
    reset_open_ai_codex_websocket_debug_stats(Some("ws-idle-before-start"));
}

/// Port of pi `openai-codex-stream.test.ts:1780-1863`; this also pins that SSE is
/// never replayed after a WebSocket event starts the assistant stream.
#[tokio::test]
async fn websocket_idle_after_start_is_terminal_without_sse_replay() {
    let server = LocalCodexServer::start(
        vec![WsAction::Events(vec![json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}
        })])],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let mut options = websocket_options(None, Transport::Auto);
    options.stream.request.timeout_ms = Some(25.0);
    let (kinds, result) = collect_event_kinds(stream(&current, &context("hello"), options)).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("WebSocket idle timeout after 25ms")
    );
    assert_eq!(kinds.iter().filter(|kind| **kind == "start").count(), 1);
    assert_eq!(kinds.last(), Some(&"error"));
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 0);
}

/// Port of pi `openai-codex-stream.test.ts:1865-1968`.
#[tokio::test]
async fn cached_websocket_max_age_forces_a_fresh_connection() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
            WsAction::Events(response_events("resp_2", "msg_2", "Again")),
        ],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let first = stream(
        &current,
        &context("hello"),
        websocket_options(Some("aged-ws-session"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("first");
    transport::age_cached_session("aged-ws-session", Duration::from_secs(56 * 60));
    let mut next = context("hello");
    next.messages.extend([
        Message::Assistant(Box::new(first)),
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("again".to_owned()),
            timestamp: 2,
        })),
    ]);
    stream(
        &current,
        &next,
        websocket_options(Some("aged-ws-session"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("second");
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    let stats = get_open_ai_codex_websocket_debug_stats("aged-ws-session").expect("stats");
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 0);
    close_open_ai_codex_websocket_sessions(Some("aged-ws-session"));
    reset_open_ai_codex_websocket_debug_stats(Some("aged-ws-session"));
}

/// Pins pi `openai-codex-responses.ts:1008-1011,1107-1123`: an idle cached
/// socket is evicted after five minutes.
#[tokio::test(start_paused = true)]
async fn cached_websocket_idle_ttl_evicts_the_connection() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
            WsAction::Events(response_events("resp_2", "msg_2", "Again")),
        ],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let idle_ttl_options = || {
        let mut options = websocket_options(Some("idle-ttl-session"), Transport::WebsocketCached);
        options.stream.websocket_connect_timeout_ms = Some(0.0);
        options
    };
    stream(&current, &context("hello"), idle_ttl_options())
        .result()
        .await
        .expect("first");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    for _ in 0..20 {
        if server
            .state
            .websocket_disconnections
            .load(Ordering::Relaxed)
            > 0
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    stream(&current, &context("again"), idle_ttl_options())
        .result()
        .await
        .expect("second");
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    let stats = get_open_ai_codex_websocket_debug_stats("idle-ttl-session").expect("stats");
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 0);
    close_open_ai_codex_websocket_sessions(Some("idle-ttl-session"));
    reset_open_ai_codex_websocket_debug_stats(Some("idle-ttl-session"));
}

/// Port of pi `openai-codex-stream.test.ts:1970-2140`.
#[tokio::test]
async fn websocket_cached_sends_only_custom_tool_result_and_user_input_delta() {
    let first_events = vec![
        json!({"type":"response.created","response":{"id":"resp_1"}}),
        json!({
            "type":"response.output_item.added","output_index":0,
            "item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"sample_tool","input":""}
        }),
        json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":"abc"}),
        json!({"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"ctc_1","input":"abc"}),
        json!({
            "type":"response.output_item.done","output_index":0,
            "item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"sample_tool","input":"abc"}
        }),
        json!({
            "type":"response.completed",
            "response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}
        }),
    ];
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(first_events),
            WsAction::Events(vec![json!({
                "type":"response.completed",
                "response":{"id":"resp_2","status":"completed","usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}
            })]),
        ],
        "unexpected SSE".to_owned(),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    current.compat = Some(ModelCompat::OpenAIResponses(OpenAIResponsesCompat {
        supports_open_ai_grammar_tools: Some(true),
        ..Default::default()
    }));
    let mut first_context = context("Use the tool");
    first_context.tools = Some(vec![Tool {
        name: "sample_tool".to_owned(),
        description: "Sample tool".to_owned(),
        parameters: json!({
            "type":"object",
            "properties":{"payload":{"type":"string"}},
            "required":["payload"]
        }),
        constrained_sampling: Some(ToolConstrainedSampling::Config(
            ConstrainedSamplingConfig::Grammar {
                variants: GrammarVariants {
                    openai_lark: Some("start: /[a-z]+/".to_owned()),
                    openai_regex: None,
                },
            },
        )),
    }]);
    let first = stream(
        &current,
        &first_context,
        websocket_options(Some("custom-delta-session"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("first");
    let mut second_context = first_context.clone();
    second_context.messages.extend([
        Message::Assistant(Box::new(first)),
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call_1|ctc_1".to_owned(),
            tool_name: "sample_tool".to_owned(),
            content: vec![crate::types::UserContentBlock::Text(TextContent::new(
                "real result",
            ))],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2,
        })),
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("Now finish".to_owned()),
            timestamp: 3,
        })),
    ]);
    stream(
        &current,
        &second_context,
        websocket_options(Some("custom-delta-session"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("second");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["store"], false);
    assert!(requests[0].get("previous_response_id").is_none());
    assert_eq!(
        requests[0]["input"],
        json!([{"role":"user","content":[{"type":"input_text","text":"Use the tool"}]}])
    );
    assert_eq!(requests[1]["store"], false);
    assert_eq!(requests[1]["previous_response_id"], "resp_1");
    assert_eq!(
        requests[1]["input"],
        json!([
            {"type":"custom_tool_call_output","call_id":"call_1","output":"real result"},
            {"role":"user","content":[{"type":"input_text","text":"Now finish"}]}
        ])
    );
    let stats = get_open_ai_codex_websocket_debug_stats("custom-delta-session").expect("stats");
    assert_eq!(stats.requests, 2);
    assert_eq!(stats.connections_created, 1);
    assert_eq!(stats.connections_reused, 1);
    assert_eq!(stats.cached_context_requests, 2);
    assert_eq!(stats.store_true_requests, 0);
    assert_eq!(stats.full_context_requests, 1);
    assert_eq!(stats.delta_requests, 1);
    assert_eq!(stats.last_delta_input_items, Some(2));
    assert_eq!(stats.last_previous_response_id.as_deref(), Some("resp_1"));
    close_open_ai_codex_websocket_sessions(Some("custom-delta-session"));
    reset_open_ai_codex_websocket_debug_stats(Some("custom-delta-session"));
}

/// Port of pi `openai-codex-stream.test.ts:2142-2358` (WebSocket recovery case).
#[tokio::test]
async fn missing_websocket_continuation_retries_once_with_full_context() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
            WsAction::Events(vec![json!({
                "type":"error",
                "error":{"code":PREVIOUS_RESPONSE_NOT_FOUND_CODE,"message":"Previous response not found"}
            })]),
            WsAction::Events(response_events("resp_2", "msg_2", "Recovered")),
        ],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let first_context = context("Say hello");
    let first = stream(
        &current,
        &first_context,
        websocket_options(Some("missing-continuation"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("first");
    let mut second_context = first_context.clone();
    second_context.messages.extend([
        Message::Assistant(Box::new(first)),
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("Now finish".to_owned()),
            timestamp: 2,
        })),
    ]);
    let second = stream(
        &current,
        &second_context,
        websocket_options(Some("missing-continuation"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("second");
    assert_eq!(message_text(&second), Some("Recovered"));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["previous_response_id"], "resp_1");
    assert!(requests[2].get("previous_response_id").is_none());
    assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    close_open_ai_codex_websocket_sessions(Some("missing-continuation"));
    reset_open_ai_codex_websocket_debug_stats(Some("missing-continuation"));
}

/// Port of pi `openai-codex-stream.test.ts:2142-2358` (SSE recovery row).
#[tokio::test]
async fn missing_websocket_continuation_retry_can_fall_back_to_sse() {
    let server = LocalCodexServer::start(
        vec![
            WsAction::Events(response_events("resp_1", "msg_1", "Hello")),
            WsAction::Events(vec![json!({
                "type":"error",
                "error":{"code":PREVIOUS_RESPONSE_NOT_FOUND_CODE,"message":"Previous response not found"}
            })]),
            WsAction::Close,
        ],
        terminal_payload("completed", None),
    )
    .await;
    let mut current = model("gpt-5.1-codex");
    current.base_url = server.base_url();
    let first_context = context("Say hello");
    let first = stream(
        &current,
        &first_context,
        websocket_options(Some("missing-continuation-sse"), Transport::WebsocketCached),
    )
    .result()
    .await
    .expect("first");
    let mut second_context = first_context.clone();
    second_context.messages.extend([
        Message::Assistant(Box::new(first)),
        Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("Now finish".to_owned()),
            timestamp: 2,
        })),
    ]);
    let (kinds, second) = collect_event_kinds(stream(
        &current,
        &second_context,
        websocket_options(Some("missing-continuation-sse"), Transport::WebsocketCached),
    ))
    .await;
    assert_eq!(message_text(&second), Some("Hello"));
    assert_eq!(kinds.iter().filter(|kind| **kind == "start").count(), 1);
    assert!(!kinds.contains(&"error"));
    assert_eq!(
        server.state.websocket_connections.load(Ordering::Relaxed),
        2
    );
    assert_eq!(server.state.http_requests.load(Ordering::Relaxed), 1);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["previous_response_id"], "resp_1");
    assert!(requests[2].get("previous_response_id").is_none());
    assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(3));
    let stats = get_open_ai_codex_websocket_debug_stats("missing-continuation-sse").expect("stats");
    assert_eq!(stats.requests, 3);
    assert_eq!(stats.connections_created, 2);
    assert_eq!(stats.connections_reused, 1);
    assert_eq!(stats.full_context_requests, 2);
    assert_eq!(stats.delta_requests, 1);
    assert_eq!(stats.websocket_failures, 1);
    assert_eq!(stats.sse_fallbacks, 1);
    reset_open_ai_codex_websocket_debug_stats(Some("missing-continuation-sse"));
}
