use agentprism_ai::{
    ApiFamily, ApiId, ApiRequestOptions, AssistantEvent, AssistantFinishReason, AssistantMessage,
    AuthError, AuthResolver, AuthSource, CacheRetention, CancellationToken, Context,
    DiagnosticErrorCode, HeaderMapSpec, HeaderTransform, HeaderTransformContext, HttpRequest,
    HttpResponse, HttpTransport, LocalAuthResolver, LocalBoxFuture, LocalHeaderTransform,
    LocalHttpResponse, LocalHttpTransport, LocalModelRuntime, LocalModels,
    LocalProviderRegistration, LocalResolveAuthRequest, LocalResolvedApiRequest, MiddlewareError,
    ModelRequest, ModelRuntime, Models, OpenAiCodexResponses, ProviderRegistration,
    ResolveAuthRequest, ResolvedApiRequest, ResolvedAuth, SendBoxFuture, SimpleGenerationOptions,
    StreamTransport, TransportError,
};
use agentprism_openai::{
    LocalOpenAiCodexResponsesTransport, LocalOpenAiCodexRetryClassifier,
    LocalOpenAiCodexWebSocketResponse, LocalOpenAiCodexWebSocketTransport,
    OpenAiCodexResponsesTransport, OpenAiCodexRetryClassifier, OpenAiCodexWebSocketConnection,
    OpenAiCodexWebSocketRequest, OpenAiCodexWebSocketResponse, OpenAiCodexWebSocketTransport,
    local_openai_codex_responses_api, local_openai_codex_responses_api_with_websocket,
    openai_codex_responses_api, openai_codex_responses_api_with_websocket,
    openai_codex_retry_policy,
};
use agentprism_openai_codex::openai_codex_models;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use url::Url;

#[derive(Default)]
struct RecordingHttp {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpTransport for RecordingHttp {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async { Ok(HttpResponse::empty(200, HeaderMap::new())) })
    }
}

enum WebSocketReply {
    Frames(Vec<Vec<u8>>),
    FramesThenError(Vec<Vec<u8>>, &'static str),
    FramesThenPending(Vec<Vec<u8>>),
    Pending,
    Error(&'static str),
}

struct RecordingWebSocket {
    requests: Mutex<Vec<OpenAiCodexWebSocketRequest>>,
    replies: Mutex<VecDeque<WebSocketReply>>,
    connection: Mutex<OpenAiCodexWebSocketConnection>,
    new_connection_calls: BTreeSet<usize>,
}

impl RecordingWebSocket {
    fn new(replies: impl IntoIterator<Item = WebSocketReply>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
            connection: Mutex::new(OpenAiCodexWebSocketConnection::new()),
            new_connection_calls: BTreeSet::new(),
        }
    }

    fn with_new_connection_on(
        replies: impl IntoIterator<Item = WebSocketReply>,
        calls: impl IntoIterator<Item = usize>,
    ) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
            connection: Mutex::new(OpenAiCodexWebSocketConnection::new()),
            new_connection_calls: calls.into_iter().collect(),
        }
    }
}

impl OpenAiCodexWebSocketTransport for RecordingWebSocket {
    fn execute(
        &self,
        request: OpenAiCodexWebSocketRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OpenAiCodexWebSocketResponse, TransportError>> {
        let call = self.requests.lock().unwrap().len();
        let mut connection = self.connection.lock().unwrap();
        if self.new_connection_calls.contains(&call) {
            *connection = OpenAiCodexWebSocketConnection::new();
        }
        let connection = connection.clone();
        let mut request = request;
        match request.body_for_connection(&connection) {
            Ok(body) => request.body = body,
            Err(error) => return Box::pin(async move { Err(error) }),
        }
        self.requests.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().pop_front().unwrap();
        Box::pin(async move {
            let body = match reply {
                WebSocketReply::Frames(frames) => {
                    Ok(Box::pin(stream::iter(frames.into_iter().map(Ok)))
                        as agentprism_ai::HttpBody)
                }
                WebSocketReply::FramesThenError(frames, code) => Ok(Box::pin(
                    stream::iter(frames.into_iter().map(Ok)).chain(stream::once(async move {
                        Err(TransportError::new(code, code))
                    })),
                )
                    as agentprism_ai::HttpBody),
                WebSocketReply::FramesThenPending(frames) => Ok(Box::pin(
                    stream::iter(frames.into_iter().map(Ok)).chain(stream::pending()),
                )
                    as agentprism_ai::HttpBody),
                WebSocketReply::Pending => {
                    Ok(Box::pin(stream::pending()) as agentprism_ai::HttpBody)
                }
                WebSocketReply::Error(code) => Err(TransportError::new(code, code)),
            }?;
            Ok(OpenAiCodexWebSocketResponse { connection, body })
        })
    }
}

#[derive(Default)]
struct LocalRecordingHttp {
    requests: RefCell<Vec<HttpRequest>>,
}

impl LocalHttpTransport for LocalRecordingHttp {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.requests.borrow_mut().push(request);
        Box::pin(async { Ok(LocalHttpResponse::empty(200, HeaderMap::new())) })
    }
}

#[derive(Default)]
struct RetryingHttp {
    requests: Mutex<Vec<HttpRequest>>,
}

impl HttpTransport for RetryingHttp {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let attempt = request.attempt;
        self.requests.lock().unwrap().push(request);
        Box::pin(async move { Ok(codex_retry_response(attempt)) })
    }
}

#[derive(Default)]
struct LocalRetryingHttp {
    requests: RefCell<Vec<HttpRequest>>,
}

impl LocalHttpTransport for LocalRetryingHttp {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let attempt = request.attempt;
        self.requests.borrow_mut().push(request);
        Box::pin(async move {
            let response = codex_retry_response(attempt);
            let mut bytes = Vec::new();
            let mut body = response.body;
            while let Some(chunk) = body.next().await {
                bytes.extend(chunk?);
            }
            Ok(LocalHttpResponse::from_bytes(
                response.status,
                response.headers,
                bytes,
            ))
        })
    }
}

fn codex_retry_response(attempt: u32) -> HttpResponse {
    if attempt < 2 {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", HeaderValue::from_static("0"));
        return HttpResponse::from_bytes(503, headers, b"service unavailable".to_vec());
    }
    HttpResponse::from_bytes(
        200,
        HeaderMap::new(),
        br#"data: {"type":"response.completed","response":{"id":"resp_retry","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#
        .to_vec(),
    )
}

struct LocalRecordingWebSocket {
    requests: RefCell<Vec<OpenAiCodexWebSocketRequest>>,
    replies: RefCell<VecDeque<WebSocketReply>>,
    connection: RefCell<OpenAiCodexWebSocketConnection>,
    new_connection_calls: BTreeSet<usize>,
}

impl LocalRecordingWebSocket {
    fn new(replies: impl IntoIterator<Item = WebSocketReply>) -> Self {
        Self {
            requests: RefCell::new(Vec::new()),
            replies: RefCell::new(replies.into_iter().collect()),
            connection: RefCell::new(OpenAiCodexWebSocketConnection::new()),
            new_connection_calls: BTreeSet::new(),
        }
    }

    fn with_new_connection_on(
        replies: impl IntoIterator<Item = WebSocketReply>,
        calls: impl IntoIterator<Item = usize>,
    ) -> Self {
        Self {
            requests: RefCell::new(Vec::new()),
            replies: RefCell::new(replies.into_iter().collect()),
            connection: RefCell::new(OpenAiCodexWebSocketConnection::new()),
            new_connection_calls: calls.into_iter().collect(),
        }
    }
}

impl LocalOpenAiCodexWebSocketTransport for LocalRecordingWebSocket {
    fn execute(
        &self,
        request: OpenAiCodexWebSocketRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalOpenAiCodexWebSocketResponse, TransportError>> {
        let call = self.requests.borrow().len();
        if self.new_connection_calls.contains(&call) {
            *self.connection.borrow_mut() = OpenAiCodexWebSocketConnection::new();
        }
        let connection = self.connection.borrow().clone();
        let mut request = request;
        match request.body_for_connection(&connection) {
            Ok(body) => request.body = body,
            Err(error) => return Box::pin(async move { Err(error) }),
        }
        self.requests.borrow_mut().push(request);
        let reply = self.replies.borrow_mut().pop_front().unwrap();
        Box::pin(async move {
            let body = match reply {
                WebSocketReply::Frames(frames) => {
                    Ok(Box::pin(stream::iter(frames.into_iter().map(Ok)))
                        as agentprism_ai::LocalHttpBody)
                }
                WebSocketReply::FramesThenError(frames, code) => Ok(Box::pin(
                    stream::iter(frames.into_iter().map(Ok)).chain(stream::once(async move {
                        Err(TransportError::new(code, code))
                    })),
                )
                    as agentprism_ai::LocalHttpBody),
                WebSocketReply::FramesThenPending(frames) => Ok(Box::pin(
                    stream::iter(frames.into_iter().map(Ok)).chain(stream::pending()),
                )
                    as agentprism_ai::LocalHttpBody),
                WebSocketReply::Pending => {
                    Ok(Box::pin(stream::pending()) as agentprism_ai::LocalHttpBody)
                }
                WebSocketReply::Error(code) => Err(TransportError::new(code, code)),
            }?;
            Ok(LocalOpenAiCodexWebSocketResponse { connection, body })
        })
    }
}

/// Architecture v2 part 2 §1.6/§2.6; pinned Pi basis:
/// `openai-codex-responses.ts:processWebSocketStream`. The explicit
/// WebSocket transport uses an uncompressed `response.create` frame and
/// credential identity wins over hostile logical overlays.
#[test]
fn codex_transport_explicit_websocket_is_selectable() {
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::Frames(vec![
        done_frame("resp_ws", Vec::new()),
    ])]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());

    let mut response = futures_executor::block_on(transport.execute(
        codex_request(
            StreamTransport::Websocket,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("WebSocket response");
    futures_executor::block_on(async { while response.body.next().await.is_some() {} });

    assert!(http.requests.lock().unwrap().is_empty());
    let requests = websocket.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.url.scheme(), "wss");
    assert_eq!(request.url.path(), "/backend-api/codex/responses");
    assert_eq!(request.headers[header::AUTHORIZATION], "Bearer credential");
    assert_eq!(request.headers["chatgpt-account-id"], "account-123");
    assert_eq!(request.headers["originator"], "pi");
    assert_eq!(request.headers[header::USER_AGENT], "pi-ai-rs/0.1.0");
    assert_eq!(request.headers["x-client-request-id"], "session-1");
    assert_eq!(request.headers["session-id"], "session-1");
    assert_eq!(
        request.headers["openai-beta"],
        "responses_websockets=2026-02-06"
    );
    assert!(request.headers.get(header::CONTENT_ENCODING).is_none());
    let frame: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(frame["type"], "response.create");
    assert_eq!(frame["input"].as_array().unwrap().len(), 1);
}

/// Architecture v2 part 2 §1.6/§2.6; pinned Pi basis:
/// `openai-codex-responses.ts:streamOpenAICodexResponsesWebSocket`. A
/// one-shot WebSocket request without cache affinity receives one generated
/// request identity in both protocol headers after all caller overlays.
#[test]
fn codex_transport_one_shot_websocket_gets_request_identity_headers() {
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::Frames(vec![
        done_frame("resp_one_shot", Vec::new()),
    ])]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http, websocket.clone());
    let mut request = codex_request(
        StreamTransport::Websocket,
        serde_json::json!([{"role":"user","content":"hello"}]),
    );
    request.session_id = None;
    request.headers.remove("session-id");
    request.headers.insert(
        "x-client-request-id",
        HeaderValue::from_static("hostile-request-id"),
    );

    let mut response =
        futures_executor::block_on(transport.execute(request, CancellationToken::new()))
            .expect("one-shot WebSocket response");
    futures_executor::block_on(async { while response.body.next().await.is_some() {} });

    let requests = websocket.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.session_id.is_none());
    let request_id = request.headers["x-client-request-id"].to_str().unwrap();
    assert_ne!(request_id, "hostile-request-id");
    assert_eq!(request.headers["session-id"], request_id);
    assert_eq!(request_id.len(), 36);
    assert_eq!(request_id.as_bytes()[14], b'7');
}

/// Architecture v2 part 2 §1.6; pinned Pi basis:
/// `openai-codex-responses.ts:buildCachedWebSocketRequestBody`. The second
/// cached request carries only the suffix and the prior response ID.
#[test]
fn codex_transport_cached_websocket_reconstructs_turn_two_delta() {
    let response_item = serde_json::json!({
        "type":"message",
        "role":"assistant",
        "content":[{"type":"output_text","text":"answer","annotations":[]}],
        "status":"completed",
        "id":"msg_1"
    });
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![
            output_item_done_frame(response_item.clone()),
            done_frame("resp_1", vec![response_item.clone()]),
        ]),
        WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    let turn_one = serde_json::json!([{"role":"user","content":"hello"}]);
    let turn_two = serde_json::json!([
        {"role":"user","content":"hello"},
        response_item,
        {"type":"function_call_output","call_id":"call_1","output":"contents"}
    ]);

    for input in [turn_one, turn_two] {
        let mut response = futures_executor::block_on(transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }

    assert!(http.requests.lock().unwrap().is_empty());
    let requests = websocket.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(first.get("previous_response_id").is_none());
    assert_eq!(second["previous_response_id"], "resp_1");
    assert_eq!(
        second["input"],
        serde_json::json!([{
            "type":"function_call_output", "call_id":"call_1", "output":"contents"
        }])
    );
}

/// Architecture v2 part 2 §1.6/§10.2; pinned Pi basis:
/// `openai-codex-responses.ts:842-854,1015-1035,1423-1439`. Continuation
/// belongs to the cached physical connection, so a socket created after idle
/// or maximum-age eviction must receive the full context without
/// `previous_response_id` in both trait families.
#[test]
fn codex_cached_continuation_is_connection_scoped_after_eviction_send_and_local() {
    let response_item = serde_json::json!({
        "type":"message",
        "role":"assistant",
        "content":[{"type":"output_text","text":"answer","annotations":[]}],
        "status":"completed",
        "id":"msg_1"
    });
    let turn_one = serde_json::json!([{"role":"user","content":"hello"}]);
    let turn_two = serde_json::json!([
        {"role":"user","content":"hello"},
        response_item.clone(),
        {"type":"function_call_output","call_id":"call_1","output":"contents"}
    ]);
    let send_websocket = Arc::new(RecordingWebSocket::with_new_connection_on(
        [
            WebSocketReply::Frames(vec![
                output_item_done_frame(response_item.clone()),
                done_frame("resp_1", vec![response_item.clone()]),
            ]),
            WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
        ],
        [1],
    ));
    let send_transport = OpenAiCodexResponsesTransport::with_websocket(
        Arc::new(RecordingHttp::default()),
        send_websocket.clone(),
    );
    for input in [turn_one.clone(), turn_two.clone()] {
        let mut response = futures_executor::block_on(send_transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("Send cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let send_requests = send_websocket.requests.lock().unwrap();
    let send_second: serde_json::Value = serde_json::from_slice(&send_requests[1].body).unwrap();
    assert!(send_second.get("previous_response_id").is_none());
    assert_eq!(send_second["input"], turn_two);
    drop(send_requests);

    let local_websocket = Rc::new(LocalRecordingWebSocket::with_new_connection_on(
        [
            WebSocketReply::Frames(vec![
                output_item_done_frame(response_item.clone()),
                done_frame("resp_1", vec![response_item]),
            ]),
            WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
        ],
        [1],
    ));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        Rc::new(LocalRecordingHttp::default()),
        local_websocket.clone(),
    );
    for input in [turn_one, turn_two.clone()] {
        let mut response = futures_executor::block_on(local_transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("local cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let local_requests = local_websocket.requests.borrow();
    let local_second: serde_json::Value = serde_json::from_slice(&local_requests[1].body).unwrap();
    assert!(local_second.get("previous_response_id").is_none());
    assert_eq!(local_second["input"], turn_two);
}

/// Architecture v2 part 2 §1.6; pinned Pi basis:
/// `openai-codex-responses.ts:processWebSocketStream` rebuilds cached
/// continuation items from the assembled assistant message. A terminal frame
/// may omit tool-call output and cannot be used as the continuation source.
#[test]
fn codex_transport_cached_websocket_uses_canonical_tool_call_continuation() {
    let canonical_call = serde_json::json!({
        "type":"function_call",
        "id":"fc_1",
        "call_id":"call_1",
        "name":"read_file",
        "arguments":"{\"path\":\"README.md\"}"
    });
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![
            output_item_done_frame(serde_json::json!({
                "type":"function_call",
                "id":"fc_1",
                "call_id":"call_1",
                "name":"read_file",
                "arguments":"{\"path\": \"README.md\"}",
                "status":"completed"
            })),
            done_frame("resp_1", Vec::new()),
        ]),
        WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http, websocket.clone());
    let inputs = [
        serde_json::json!([{"role":"user","content":"hello"}]),
        serde_json::json!([
            {"role":"user","content":"hello"},
            canonical_call,
            {"type":"function_call_output","call_id":"call_1","output":"contents"}
        ]),
    ];
    for input in inputs {
        let mut response = futures_executor::block_on(transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let requests = websocket.requests.lock().unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_1");
    assert_eq!(
        second["input"],
        serde_json::json!([{
            "type":"function_call_output", "call_id":"call_1", "output":"contents"
        }])
    );
}

/// Local trait-family realization of
/// `codex_transport_cached_websocket_uses_canonical_tool_call_continuation`.
#[test]
fn codex_local_transport_cached_websocket_uses_canonical_tool_call_continuation() {
    let canonical_call = serde_json::json!({
        "type":"function_call",
        "id":"fc_1",
        "call_id":"call_1",
        "name":"read_file",
        "arguments":"{\"path\":\"README.md\"}"
    });
    let http = Rc::new(LocalRecordingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![
            output_item_done_frame(serde_json::json!({
                "type":"function_call",
                "id":"fc_1",
                "call_id":"call_1",
                "name":"read_file",
                "arguments":"{\"path\": \"README.md\"}",
                "status":"completed"
            })),
            done_frame("resp_1", Vec::new()),
        ]),
        WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
    ]));
    let transport = LocalOpenAiCodexResponsesTransport::with_websocket(http, websocket.clone());
    let inputs = [
        serde_json::json!([{"role":"user","content":"hello"}]),
        serde_json::json!([
            {"role":"user","content":"hello"},
            canonical_call,
            {"type":"function_call_output","call_id":"call_1","output":"contents"}
        ]),
    ];
    for input in inputs {
        let mut response = futures_executor::block_on(transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("local cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let requests = websocket.requests.borrow();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_1");
    assert_eq!(
        second["input"],
        serde_json::json!([{
            "type":"function_call_output", "call_id":"call_1", "output":"contents"
        }])
    );
}

/// Architecture v2 part 2 §1.6; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` and
/// `openai-codex-responses.ts:processWebSocketStream`. Cached continuation is
/// rebuilt from the assembled assistant, so absent and empty terminal
/// arguments retain the streamed function-call arguments.
#[test]
fn codex_transport_cached_websocket_preserves_streamed_function_arguments() {
    for terminal_arguments in [None, Some("")] {
        assert_send_cached_tool_continuation(
            function_call_stream_frames(terminal_arguments),
            serde_json::json!({
                "type":"function_call",
                "id":"fc_streamed",
                "call_id":"call_streamed",
                "name":"read_file",
                "arguments":"{\"path\":\"README.md\"}"
            }),
        );
    }
}

/// Local trait-family realization of
/// `codex_transport_cached_websocket_preserves_streamed_function_arguments`.
#[test]
fn codex_local_transport_cached_websocket_preserves_streamed_function_arguments() {
    for terminal_arguments in [None, Some("")] {
        assert_local_cached_tool_continuation(
            function_call_stream_frames(terminal_arguments),
            serde_json::json!({
                "type":"function_call",
                "id":"fc_streamed",
                "call_id":"call_streamed",
                "name":"read_file",
                "arguments":"{\"path\":\"README.md\"}"
            }),
        );
    }
}

/// Architecture v2 part 2 §1.6; pinned Pi basis:
/// `openai-responses-shared.ts:processResponsesStream` and
/// `openai-codex-responses.ts:processWebSocketStream`. A terminal custom-tool
/// item may omit `input`; cached continuation retains its streamed input.
#[test]
fn codex_transport_cached_websocket_preserves_streamed_custom_input() {
    assert_send_cached_tool_continuation(
        custom_tool_stream_frames(),
        serde_json::json!({
            "type":"custom_tool_call",
            "id":"ctc_streamed",
            "call_id":"call_custom",
            "name":"query",
            "input":"hello"
        }),
    );
}

/// Local trait-family realization of
/// `codex_transport_cached_websocket_preserves_streamed_custom_input`.
#[test]
fn codex_local_transport_cached_websocket_preserves_streamed_custom_input() {
    assert_local_cached_tool_continuation(
        custom_tool_stream_frames(),
        serde_json::json!({
            "type":"custom_tool_call",
            "id":"ctc_streamed",
            "call_id":"call_custom",
            "name":"query",
            "input":"hello"
        }),
    );
}

/// Architecture v2 part 2 §1.6; pinned Pi basis: Codex transport selection
/// falls back to SSE only for a pre-stream WebSocket transport failure and
/// records that fallback for the session.
#[test]
fn codex_transport_auto_falls_back_before_stream_and_sticks_to_sse() {
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::Error("connect")]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());

    for index in 0..2 {
        let response = futures_executor::block_on(transport.execute(
            codex_request(
                StreamTransport::Auto,
                serde_json::json!([{"role":"user","content":"hello"}]),
            ),
            CancellationToken::new(),
        ))
        .expect("SSE fallback");
        if index == 0 {
            let diagnostic = response.diagnostics.first().expect("fallback diagnostic");
            assert_eq!(diagnostic.kind, "provider_transport_failure");
            assert_eq!(
                diagnostic.error.as_ref().unwrap().code.as_ref(),
                Some(&DiagnosticErrorCode::String("connect".into()))
            );
            assert_eq!(diagnostic.details["configuredTransport"], "auto");
            assert_eq!(diagnostic.details["fallbackTransport"], "sse");
            assert_eq!(diagnostic.details["eventsEmitted"], false);
            assert_eq!(diagnostic.details["phase"], "before_message_stream_start");
        } else {
            assert!(response.diagnostics.is_empty());
        }
    }

    assert_eq!(websocket.requests.lock().unwrap().len(), 1);
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.url.path() == "/backend-api/codex/responses"
            && request.headers[header::AUTHORIZATION] == "Bearer credential"
            && request.headers[header::CONTENT_ENCODING] == "zstd"
    }));
}

/// Architecture v2 part 2 §1.6/§9.2; local counterpart to the Pi fallback
/// diagnostic and sticky-session behavior.
#[test]
fn codex_local_transport_auto_fallback_emits_diagnostic_and_sticks_to_sse() {
    let http = Rc::new(LocalRecordingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([WebSocketReply::Error(
        "connect",
    )]));
    let transport =
        LocalOpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    for index in 0..2 {
        let response = futures_executor::block_on(transport.execute(
            codex_request(
                StreamTransport::Auto,
                serde_json::json!([{"role":"user","content":"hello"}]),
            ),
            CancellationToken::new(),
        ))
        .expect("local SSE fallback");
        if index == 0 {
            let diagnostic = response.diagnostics.first().expect("fallback diagnostic");
            assert_eq!(diagnostic.kind, "provider_transport_failure");
            assert_eq!(
                diagnostic.error.as_ref().unwrap().code.as_ref(),
                Some(&DiagnosticErrorCode::String("connect".into()))
            );
            assert_eq!(diagnostic.details["configuredTransport"], "auto");
            assert_eq!(diagnostic.details["fallbackTransport"], "sse");
            assert_eq!(diagnostic.details["eventsEmitted"], false);
        } else {
            assert!(response.diagnostics.is_empty());
        }
    }
    assert_eq!(websocket.requests.borrow().len(), 1);
    assert_eq!(http.requests.borrow().len(), 2);
}

/// Architecture v2 part 2 §1.6/§2.6; pinned Pi basis:
/// `openai-codex-responses.ts:resolveCodexUrl` resolves an existing `/codex`
/// suffix without duplicating it for either transport.
#[test]
fn codex_responses_base_url_resolution_matches_pi() {
    let http = Arc::new(RecordingHttp::default());
    let transport = OpenAiCodexResponsesTransport::new(http.clone());
    let mut sse_request = codex_request(
        StreamTransport::Sse,
        serde_json::json!([{"role":"user","content":"hello"}]),
    );
    sse_request.url = Url::parse("https://chatgpt.com/backend-api/codex").unwrap();
    futures_executor::block_on(transport.execute(sse_request, CancellationToken::new()))
        .expect("SSE URL resolution");
    assert_eq!(
        http.requests.lock().unwrap()[0].url.path(),
        "/backend-api/codex/responses"
    );

    let websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::Frames(vec![
        done_frame("resp_url", Vec::new()),
    ])]));
    let websocket_transport = OpenAiCodexResponsesTransport::with_websocket(
        Arc::new(RecordingHttp::default()),
        websocket.clone(),
    );
    let mut websocket_request = codex_request(
        StreamTransport::Websocket,
        serde_json::json!([{"role":"user","content":"hello"}]),
    );
    websocket_request.url = Url::parse("https://chatgpt.com/backend-api/codex").unwrap();
    let mut response = futures_executor::block_on(
        websocket_transport.execute(websocket_request, CancellationToken::new()),
    )
    .expect("WebSocket URL resolution");
    futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    let request = &websocket.requests.lock().unwrap()[0];
    assert_eq!(request.url.scheme(), "wss");
    assert_eq!(request.url.path(), "/backend-api/codex/responses");
}

/// Architecture v2 part 2 §2.6/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:1614-1631` sets option-derived Codex affinity
/// headers after model, caller, and transformed overlays on the SSE path.
#[test]
fn responses_codex_session_affinity_reasserted_send_and_local() {
    let send_transport = Arc::new(RecordingHttp::default());
    let send_model = openai_codex_models().unwrap().remove(0);
    let send_registration = ProviderRegistration::builder("openai-codex")
        .base_url(send_model.common.base_url.clone())
        .auth(Arc::new(CodexAuth))
        .models(vec![send_model.clone()])
        .api(
            OpenAiCodexResponses::API_ID,
            openai_codex_responses_api(send_transport.clone()),
        )
        .build()
        .expect("Send Codex registration");
    let send_models = Models::builder()
        .provider(send_registration)
        .header_transform(Arc::new(HostileCodexAffinity))
        .build()
        .expect("Send Codex Models");
    let send_stream = futures_executor::block_on(ModelRuntime::stream(
        &send_models,
        codex_affinity_request(&send_model),
        CancellationToken::new(),
    ))
    .expect("Send Codex stream");
    futures_executor::block_on(async { send_stream.collect::<Vec<_>>().await });
    assert_selected_codex_affinity(&send_transport.requests.lock().unwrap()[0].headers);

    let local_transport = Rc::new(LocalRecordingHttp::default());
    let local_model = openai_codex_models().unwrap().remove(0);
    let local_registration = LocalProviderRegistration::builder("openai-codex")
        .base_url(local_model.common.base_url.clone())
        .auth(Rc::new(CodexAuth))
        .models(vec![local_model.clone()])
        .api(
            OpenAiCodexResponses::API_ID,
            local_openai_codex_responses_api(local_transport.clone()),
        )
        .build()
        .expect("local Codex registration");
    let local_models = LocalModels::builder()
        .provider(local_registration)
        .header_transform(Rc::new(LocalHostileCodexAffinity))
        .build()
        .expect("local Codex Models");
    let local_stream = futures_executor::block_on(LocalModelRuntime::stream(
        &local_models,
        codex_affinity_request(&local_model),
        CancellationToken::new(),
    ))
    .expect("local Codex stream");
    futures_executor::block_on(async { local_stream.collect::<Vec<_>>().await });
    assert_selected_codex_affinity(&local_transport.requests.borrow()[0].headers);
}

struct CodexAuth;

impl AuthResolver for CodexAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(Some(codex_auth())) })
    }
}

impl LocalAuthResolver for CodexAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(Some(codex_auth())) })
    }
}

fn codex_auth() -> ResolvedAuth {
    ResolvedAuth {
        api_key: None,
        headers: retry_auth_headers(),
        transport_headers: HeaderMap::new(),
        base_url: None,
        source: AuthSource::new("fixture"),
    }
}

struct HostileCodexAffinity;

impl HeaderTransform for HostileCodexAffinity {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        apply_hostile_codex_affinity(headers);
        Box::pin(async { Ok(()) })
    }
}

struct LocalHostileCodexAffinity;

impl LocalHeaderTransform for LocalHostileCodexAffinity {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        apply_hostile_codex_affinity(headers);
        Box::pin(async { Ok(()) })
    }
}

fn apply_hostile_codex_affinity(headers: &mut HeaderMap) {
    headers.remove("session-id");
    headers.insert(
        "x-client-request-id",
        HeaderValue::from_static("hostile-transform"),
    );
}

fn codex_affinity_request(model: &agentprism_ai::ModelDescriptor) -> ModelRequest {
    let mut headers = HeaderMapSpec::new();
    headers.insert("session-id".into(), None);
    headers.insert(
        "x-client-request-id".into(),
        Some("hostile-explicit".into()),
    );
    ModelRequest {
        model: model.common.model_ref.clone(),
        context: Context::new(None),
        options: SimpleGenerationOptions {
            session_id: Some("selected-session".into()),
            cache_retention: Some(CacheRetention::Short),
            transport: Some(StreamTransport::Sse),
            headers,
            ..Default::default()
        },
    }
}

fn assert_selected_codex_affinity(headers: &HeaderMap) {
    assert_eq!(headers["session-id"], "selected-session");
    assert_eq!(headers["x-client-request-id"], "selected-session");
}

/// Architecture v2 part 2 §2.4/§2.6; pinned Pi keeps a pre-stream
/// WebSocket-to-SSE fallback for the entire logical request. With no session
/// ID, the initial WebSocket is still attempted only once and transport
/// retries remain SSE-only.
#[test]
fn codex_transport_fallback_stays_sse_across_retries_without_session() {
    let http = Arc::new(RetryingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::Error("connect")]));
    let api = openai_codex_responses_api_with_websocket(http.clone(), websocket.clone());
    let response =
        futures_executor::block_on(api.stream(send_retry_request(), CancellationToken::new()))
            .expect("SSE retry establishes a stream");
    let events = futures_executor::block_on(async { response.collect::<Vec<_>>().await });
    let terminal = terminal_message(&events);
    assert_eq!(terminal.diagnostics.len(), 1);
    assert_eq!(terminal.diagnostics[0].details["fallbackTransport"], "sse");

    assert_eq!(websocket.requests.lock().unwrap().len(), 1);
    let requests = http.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.attempt)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("session-id").is_none())
    );
}

/// Local trait-family realization of
/// `codex_transport_fallback_stays_sse_across_retries_without_session`.
#[test]
fn codex_local_transport_fallback_stays_sse_across_retries_without_session() {
    let http = Rc::new(LocalRetryingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([WebSocketReply::Error(
        "connect",
    )]));
    let api = local_openai_codex_responses_api_with_websocket(http.clone(), websocket.clone());
    let response =
        futures_executor::block_on(api.stream(local_retry_request(), CancellationToken::new()))
            .expect("local SSE retry establishes a stream");
    let events = futures_executor::block_on(async { response.collect::<Vec<_>>().await });
    let terminal = terminal_message(&events);
    assert_eq!(terminal.diagnostics.len(), 1);
    assert_eq!(terminal.diagnostics[0].details["fallbackTransport"], "sse");

    assert_eq!(websocket.requests.borrow().len(), 1);
    let requests = http.requests.borrow();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.attempt)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("session-id").is_none())
    );
}

/// Pinned Pi basis: `openai-codex-stream.test.ts` retries a missing cached
/// continuation once before accepting semantic output on the replacement
/// WebSocket.
#[test]
fn codex_transport_retries_missing_continuation_before_stream_start() {
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![error_frame("previous_response_not_found")]),
        WebSocketReply::Frames(vec![done_frame("resp_retry", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());

    let mut response = futures_executor::block_on(transport.execute(
        codex_request(
            StreamTransport::WebsocketCached,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("missing continuation retry");
    futures_executor::block_on(async { while response.body.next().await.is_some() {} });

    assert_eq!(websocket.requests.lock().unwrap().len(), 2);
    assert!(http.requests.lock().unwrap().is_empty());
}

/// Architecture v2 part 2 §1.6/§9.2/§10.8; pinned Pi basis:
/// `openai-codex-stream.test.ts` emits `codex.rate_limits` before a nested
/// `previous_response_not_found` error and retries in both trait families.
#[test]
fn codex_transport_retries_missing_continuation_after_rate_limits_send_and_local() {
    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![
            rate_limits_frame(),
            nested_error_frame("previous_response_not_found"),
        ]),
        WebSocketReply::Frames(vec![done_frame("resp_send", Vec::new())]),
    ]));
    let send_transport =
        OpenAiCodexResponsesTransport::with_websocket(send_http.clone(), send_websocket.clone());
    let mut send_response = futures_executor::block_on(send_transport.execute(
        codex_request(
            StreamTransport::WebsocketCached,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("Send missing-continuation retry");
    assert!(!send_response.notify_observers);
    futures_executor::block_on(async { while send_response.body.next().await.is_some() {} });
    assert_eq!(send_websocket.requests.lock().unwrap().len(), 2);
    assert!(send_http.requests.lock().unwrap().is_empty());

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![
            rate_limits_frame(),
            nested_error_frame("previous_response_not_found"),
        ]),
        WebSocketReply::Frames(vec![done_frame("resp_local", Vec::new())]),
    ]));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        local_http.clone(),
        local_websocket.clone(),
    );
    let mut local_response = futures_executor::block_on(local_transport.execute(
        codex_request(
            StreamTransport::WebsocketCached,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("local missing-continuation retry");
    assert!(!local_response.notify_observers);
    futures_executor::block_on(async { while local_response.body.next().await.is_some() {} });
    assert_eq!(local_websocket.requests.borrow().len(), 2);
    assert!(local_http.requests.borrow().is_empty());
}

/// Architecture v2 part 2 §1.6/§9.2/§10.8; pinned Pi basis:
/// `openai-codex-responses.ts:processWebSocketStream` reconstructs a cached
/// baseline from assembled output-item events, never terminal `response.output`.
#[test]
fn codex_cached_continuation_ignores_terminal_only_output_send_and_local() {
    let phantom = serde_json::json!({
        "type":"message",
        "role":"assistant",
        "content":[{"type":"output_text","text":"phantom","annotations":[]}],
        "status":"completed",
        "id":"msg_phantom"
    });
    let turn_one = serde_json::json!([{"role":"user","content":"hello"}]);
    let turn_two = serde_json::json!([
        {"role":"user","content":"hello"},
        {"role":"user","content":"next"}
    ]);

    let send_websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_1", vec![phantom.clone()])]),
        WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
    ]));
    let send_transport = OpenAiCodexResponsesTransport::with_websocket(
        Arc::new(RecordingHttp::default()),
        send_websocket.clone(),
    );
    for input in [turn_one.clone(), turn_two.clone()] {
        let mut response = futures_executor::block_on(send_transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .unwrap();
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let send_requests = send_websocket.requests.lock().unwrap();
    let send_second: serde_json::Value = serde_json::from_slice(&send_requests[1].body).unwrap();
    assert_eq!(send_second["previous_response_id"], "resp_1");
    assert_eq!(
        send_second["input"],
        serde_json::json!([{"role":"user","content":"next"}])
    );

    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_1", vec![phantom])]),
        WebSocketReply::Frames(vec![done_frame("resp_2", Vec::new())]),
    ]));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        Rc::new(LocalRecordingHttp::default()),
        local_websocket.clone(),
    );
    for input in [turn_one, turn_two] {
        let mut response = futures_executor::block_on(local_transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .unwrap();
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let local_requests = local_websocket.requests.borrow();
    let local_second: serde_json::Value = serde_json::from_slice(&local_requests[1].body).unwrap();
    assert_eq!(local_second["previous_response_id"], "resp_1");
    assert_eq!(
        local_second["input"],
        serde_json::json!([{"role":"user","content":"next"}])
    );
}

/// Pinned Pi basis: `openai-codex-stream.test.ts` retries a pre-output
/// connection-limit response once, then falls back to SSE for the session.
#[test]
fn codex_transport_connection_limit_retries_once_then_falls_back() {
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![error_frame("websocket_connection_limit_reached")]),
        WebSocketReply::Frames(vec![error_frame("websocket_connection_limit_reached")]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());

    futures_executor::block_on(transport.execute(
        codex_request(
            StreamTransport::Auto,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("connection-limit SSE fallback");

    assert_eq!(websocket.requests.lock().unwrap().len(), 2);
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

/// Pinned Pi basis: `openai-codex-stream.test.ts` falls back when WebSocket
/// idleness occurs before the first event, but never after output has begun.
#[test]
fn codex_transport_idle_fallback_is_limited_to_pre_stream() {
    let http = Arc::new(RecordingHttp::default());
    let pending = Arc::new(RecordingWebSocket::new([WebSocketReply::Pending]));
    let pending_transport =
        OpenAiCodexResponsesTransport::with_websocket(http.clone(), pending.clone());
    let mut request = codex_request(
        StreamTransport::Auto,
        serde_json::json!([{"role":"user","content":"hello"}]),
    );
    request.timeout = Some(std::time::Duration::from_millis(5));
    futures_executor::block_on(pending_transport.execute(request, CancellationToken::new()))
        .expect("pre-stream idle SSE fallback");
    assert_eq!(http.requests.lock().unwrap().len(), 1);

    let after_start_http = Arc::new(RecordingHttp::default());
    let after_start = Arc::new(RecordingWebSocket::new([WebSocketReply::FramesThenError(
        vec![
            serde_json::to_vec(&serde_json::json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_1","role":"assistant","content":[]}
            }))
            .unwrap(),
        ],
        "idle_after_start",
    )]));
    let after_start_transport =
        OpenAiCodexResponsesTransport::with_websocket(after_start_http.clone(), after_start);
    let mut response = futures_executor::block_on(after_start_transport.execute(
        codex_request(
            StreamTransport::Auto,
            serde_json::json!([{"role":"user","content":"hello"}]),
        ),
        CancellationToken::new(),
    ))
    .expect("established WebSocket stream");
    assert!(
        futures_executor::block_on(response.body.next())
            .unwrap()
            .is_ok()
    );
    assert!(
        futures_executor::block_on(response.body.next())
            .unwrap()
            .is_err()
    );
    assert!(after_start_http.requests.lock().unwrap().is_empty());
}

/// Architecture v2 part 2 §1.6/§2.6/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:267-288,316,1614-1631` derives WebSocket
/// affinity and continuation state from typed `sessionId`/cache options, not
/// from same-named model or caller headers.
#[test]
fn codex_logical_session_headers_do_not_enable_typed_state_send_and_local() {
    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_send_1", Vec::new())]),
        WebSocketReply::Frames(vec![done_frame("resp_send_2", Vec::new())]),
    ]));
    let send_transport =
        OpenAiCodexResponsesTransport::with_websocket(send_http, send_websocket.clone());
    for prompt in ["first", "second"] {
        let mut request = codex_request(
            StreamTransport::WebsocketCached,
            serde_json::json!([{"role":"user","content":prompt}]),
        );
        request.session_id = None;
        let mut response =
            futures_executor::block_on(send_transport.execute(request, CancellationToken::new()))
                .expect("Send one-shot WebSocket");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let send_requests = send_websocket.requests.lock().unwrap();
    assert_eq!(send_requests.len(), 2);
    assert!(
        send_requests
            .iter()
            .all(|request| request.session_id.is_none())
    );
    assert!(send_requests.iter().all(|request| {
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["previous_response_id"]
            .is_null()
    }));

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_local_1", Vec::new())]),
        WebSocketReply::Frames(vec![done_frame("resp_local_2", Vec::new())]),
    ]));
    let local_transport =
        LocalOpenAiCodexResponsesTransport::with_websocket(local_http, local_websocket.clone());
    for prompt in ["first", "second"] {
        let mut request = codex_request(
            StreamTransport::WebsocketCached,
            serde_json::json!([{"role":"user","content":prompt}]),
        );
        request.session_id = None;
        let mut response =
            futures_executor::block_on(local_transport.execute(request, CancellationToken::new()))
                .expect("local one-shot WebSocket");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let local_requests = local_websocket.requests.borrow();
    assert_eq!(local_requests.len(), 2);
    assert!(
        local_requests
            .iter()
            .all(|request| request.session_id.is_none())
    );
    assert!(local_requests.iter().all(|request| {
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["previous_response_id"]
            .is_null()
    }));
}

/// Architecture v2 part 2 §1.6/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:333-363,930-949` records a post-start
/// WebSocket body failure for the typed session. The current stream fails in
/// place, while the next request for that session starts directly on SSE.
#[test]
fn codex_post_start_failure_sticks_next_typed_session_to_sse_send_and_local() {
    let started = serde_json::to_vec(&serde_json::json!({
        "type":"codex.rate_limits",
        "limits":[]
    }))
    .unwrap();

    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::FramesThenError(
        vec![started.clone()],
        "after_start",
    )]));
    let send_transport =
        OpenAiCodexResponsesTransport::with_websocket(send_http.clone(), send_websocket.clone());
    let mut first = futures_executor::block_on(send_transport.execute(
        codex_request(StreamTransport::Auto, serde_json::json!([])),
        CancellationToken::new(),
    ))
    .expect("Send established WebSocket");
    let send_items = futures_executor::block_on(async {
        let mut items = Vec::new();
        while let Some(item) = first.body.next().await {
            items.push(item);
        }
        items
    });
    assert!(send_items.last().is_some_and(Result::is_err));
    futures_executor::block_on(send_transport.execute(
        codex_request(StreamTransport::Auto, serde_json::json!([])),
        CancellationToken::new(),
    ))
    .expect("Send sticky SSE request");
    assert_eq!(send_websocket.requests.lock().unwrap().len(), 1);
    assert_eq!(send_http.requests.lock().unwrap().len(), 1);

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::FramesThenError(vec![started], "after_start"),
    ]));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        local_http.clone(),
        local_websocket.clone(),
    );
    let mut first = futures_executor::block_on(local_transport.execute(
        codex_request(StreamTransport::Auto, serde_json::json!([])),
        CancellationToken::new(),
    ))
    .expect("local established WebSocket");
    let local_items = futures_executor::block_on(async {
        let mut items = Vec::new();
        while let Some(item) = first.body.next().await {
            items.push(item);
        }
        items
    });
    assert!(local_items.last().is_some_and(Result::is_err));
    futures_executor::block_on(local_transport.execute(
        codex_request(StreamTransport::Auto, serde_json::json!([])),
        CancellationToken::new(),
    ))
    .expect("local sticky SSE request");
    assert_eq!(local_websocket.requests.borrow().len(), 1);
    assert_eq!(local_http.requests.borrow().len(), 1);
}

/// Architecture v2 part 2 §1.6/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:267-275,930-949,1466-1486` keeps the raw typed
/// `cacheSessionId` as its internal cache/fallback key and clamps only the
/// protocol-facing Codex session/request identifier. Distinct long option
/// values that share their first 64 characters must not share sticky state.
#[test]
fn codex_typed_session_state_uses_unclamped_option_key_send_and_local() {
    let prefix = "x".repeat(64);
    let failed_session = format!("{prefix}-failed");
    let other_session = format!("{prefix}-other");
    let started = serde_json::to_vec(&serde_json::json!({
        "type":"codex.rate_limits",
        "limits":[]
    }))
    .unwrap();

    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::FramesThenError(vec![started.clone()], "after_start"),
        WebSocketReply::Frames(vec![done_frame("resp_other", Vec::new())]),
    ]));
    let send_transport =
        OpenAiCodexResponsesTransport::with_websocket(send_http.clone(), send_websocket.clone());
    let mut failed_request = codex_request(StreamTransport::Auto, serde_json::json!([]));
    failed_request.session_id = Some(failed_session.clone());
    let mut failed = futures_executor::block_on(
        send_transport.execute(failed_request, CancellationToken::new()),
    )
    .expect("Send established WebSocket");
    futures_executor::block_on(async { while failed.body.next().await.is_some() {} });
    let mut other_request = codex_request(StreamTransport::Auto, serde_json::json!([]));
    other_request.session_id = Some(other_session.clone());
    let mut other =
        futures_executor::block_on(send_transport.execute(other_request, CancellationToken::new()))
            .expect("Send distinct raw session remains WebSocket-eligible");
    futures_executor::block_on(async { while other.body.next().await.is_some() {} });
    assert_eq!(send_websocket.requests.lock().unwrap().len(), 2);
    assert!(send_http.requests.lock().unwrap().is_empty());

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::FramesThenError(vec![started], "after_start"),
        WebSocketReply::Frames(vec![done_frame("resp_local_other", Vec::new())]),
    ]));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        local_http.clone(),
        local_websocket.clone(),
    );
    let mut failed_request = codex_request(StreamTransport::Auto, serde_json::json!([]));
    failed_request.session_id = Some(failed_session);
    let mut failed = futures_executor::block_on(
        local_transport.execute(failed_request, CancellationToken::new()),
    )
    .expect("local established WebSocket");
    futures_executor::block_on(async { while failed.body.next().await.is_some() {} });
    let mut other_request = codex_request(StreamTransport::Auto, serde_json::json!([]));
    other_request.session_id = Some(other_session);
    let mut other = futures_executor::block_on(
        local_transport.execute(other_request, CancellationToken::new()),
    )
    .expect("local distinct raw session remains WebSocket-eligible");
    futures_executor::block_on(async { while other.body.next().await.is_some() {} });
    assert_eq!(local_websocket.requests.borrow().len(), 2);
    assert!(local_http.requests.borrow().is_empty());
}

/// Architecture v2 part 2 §10.1 `stream_missing_provider_terminal_fails`
/// and §9.2; pinned Pi basis: `openai-codex-responses.ts:parseWebSocket` and
/// `processWebSocketStream`. Clean WebSocket EOF after semantic output fails
/// in place, clears continuation, records the transport diagnostic, and makes
/// the typed session sticky to SSE in both trait families.
#[test]
fn stream_missing_provider_terminal_fails() {
    let partial_frames = partial_message_frames();
    let first_input = serde_json::json!([{"role":"user","content":"first"}]);
    let second_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"}
    ]);
    let third_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"},
        {"role":"user","content":"third"}
    ]);

    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::Frames(partial_frames.clone()),
    ]));
    let send_transport =
        OpenAiCodexResponsesTransport::with_websocket(send_http.clone(), send_websocket.clone());
    consume_send_response(
        &send_transport,
        codex_request(StreamTransport::Auto, first_input.clone()),
        CancellationToken::new(),
    );
    let send_items = collect_send_response(
        &send_transport,
        codex_request(StreamTransport::Auto, second_input.clone()),
        CancellationToken::new(),
    );
    assert_clean_eof_failure(send_items.last().expect("Send clean EOF failure"));
    consume_send_response(
        &send_transport,
        codex_request(StreamTransport::Auto, third_input.clone()),
        CancellationToken::new(),
    );
    assert_eq!(send_websocket.requests.lock().unwrap().len(), 2);
    let send_http_requests = send_http.requests.lock().unwrap();
    assert_eq!(send_http_requests.len(), 1);
    assert_eq!(
        decoded_codex_http_request(&send_http_requests[0])["input"],
        third_input
    );

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::Frames(partial_frames),
    ]));
    let local_transport = LocalOpenAiCodexResponsesTransport::with_websocket(
        local_http.clone(),
        local_websocket.clone(),
    );
    consume_local_response(
        &local_transport,
        codex_request(StreamTransport::Auto, first_input),
        CancellationToken::new(),
    );
    let local_items = collect_local_response(
        &local_transport,
        codex_request(StreamTransport::Auto, second_input),
        CancellationToken::new(),
    );
    assert_clean_eof_failure(local_items.last().expect("local clean EOF failure"));
    consume_local_response(
        &local_transport,
        codex_request(StreamTransport::Auto, third_input.clone()),
        CancellationToken::new(),
    );
    assert_eq!(local_websocket.requests.borrow().len(), 2);
    let local_http_requests = local_http.requests.borrow();
    assert_eq!(local_http_requests.len(), 1);
    assert_eq!(
        decoded_codex_http_request(&local_http_requests[0])["input"],
        third_input
    );
}

/// Architecture v2 part 2 §10.1 `stream_cancellation_is_terminal_message`
/// and §9.2; pinned Pi basis: `openai-codex-responses.ts:processWebSocketStream`
/// clears cached continuation on abort without enabling sticky SSE fallback.
/// The decoder owns cancellation and drops the body adapter, so both guard
/// implementations must make the next same-session WebSocket send full context.
#[test]
fn stream_cancellation_is_terminal_message() {
    assert_send_cancellation_clears_continuation();
    assert_local_cancellation_clears_continuation();
}

/// Architecture v2 part 2 §1.6/§10.2
/// `responses_codex_error_mapping_matches_pi`; pinned Pi basis:
/// `openai-codex-responses.ts:mapCodexEvents` and
/// `processWebSocketStream`. Top-level `error` and `response.failed` are
/// semantic failures even when their optional provider code is absent.
#[test]
fn responses_codex_error_mapping_matches_pi() {
    for error_frame in semantic_error_frames_without_codes() {
        assert_send_semantic_error_clears_continuation(error_frame.clone());
        assert_local_semantic_error_clears_continuation(error_frame);
    }
}

/// Architecture v2 part 2 §10.1 `stream_failure_is_terminal_message` and
/// §9.2; pinned Pi basis: `openai-codex-responses.ts:348-361` commits a
/// post-start WebSocket body failure diagnostic without an SSE fallback.
#[test]
fn stream_failure_is_terminal_message() {
    let frames = vec![
        serde_json::to_vec(&serde_json::json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"message","id":"msg_1","role":"assistant","content":[]}
        }))
        .unwrap(),
        serde_json::to_vec(&serde_json::json!({
            "type":"response.output_text.delta",
            "output_index":0,
            "delta":"partial"
        }))
        .unwrap(),
    ];

    let send_http = Arc::new(RecordingHttp::default());
    let send_websocket = Arc::new(RecordingWebSocket::new([WebSocketReply::FramesThenError(
        frames.clone(),
        "idle_after_start",
    )]));
    let send_api = openai_codex_responses_api_with_websocket(send_http.clone(), send_websocket);
    let send_events = futures_executor::block_on(async {
        send_api
            .stream(send_retry_request(), CancellationToken::new())
            .await
            .expect("Send WebSocket stream")
            .collect::<Vec<_>>()
            .await
    });
    assert_post_start_failure(terminal_message(&send_events));
    assert!(send_http.requests.lock().unwrap().is_empty());

    let local_http = Rc::new(LocalRecordingHttp::default());
    let local_websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::FramesThenError(frames, "idle_after_start"),
    ]));
    let local_api =
        local_openai_codex_responses_api_with_websocket(local_http.clone(), local_websocket);
    let local_events = futures_executor::block_on(async {
        local_api
            .stream(local_retry_request(), CancellationToken::new())
            .await
            .expect("local WebSocket stream")
            .collect::<Vec<_>>()
            .await
    });
    assert_post_start_failure(terminal_message(&local_events));
    assert!(local_http.requests.borrow().is_empty());
}

fn assert_post_start_failure(message: &AssistantMessage) {
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(message.diagnostics.len(), 1);
    let diagnostic = &message.diagnostics[0];
    assert_eq!(diagnostic.kind, "provider_transport_failure");
    assert_eq!(diagnostic.details["configuredTransport"], "auto");
    assert_eq!(diagnostic.details["eventsEmitted"], true);
    assert_eq!(diagnostic.details["phase"], "after_message_stream_start");
    assert!(!diagnostic.details.contains_key("fallbackTransport"));
}

fn terminal_message(events: &[AssistantEvent]) -> &AssistantMessage {
    events
        .iter()
        .rev()
        .find_map(AssistantEvent::terminal_message)
        .expect("terminal assistant message")
}

fn consume_send_response(
    transport: &OpenAiCodexResponsesTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) {
    let items = collect_send_response(transport, request, cancellation);
    assert!(items.iter().all(Result::is_ok));
}

fn collect_send_response(
    transport: &OpenAiCodexResponsesTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Vec<Result<Vec<u8>, TransportError>> {
    let mut response = futures_executor::block_on(transport.execute(request, cancellation))
        .expect("Send Codex response");
    futures_executor::block_on(async {
        let mut items = Vec::new();
        while let Some(item) = response.body.next().await {
            items.push(item);
        }
        items
    })
}

fn consume_local_response(
    transport: &LocalOpenAiCodexResponsesTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) {
    let items = collect_local_response(transport, request, cancellation);
    assert!(items.iter().all(Result::is_ok));
}

fn collect_local_response(
    transport: &LocalOpenAiCodexResponsesTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Vec<Result<Vec<u8>, TransportError>> {
    let mut response = futures_executor::block_on(transport.execute(request, cancellation))
        .expect("local Codex response");
    futures_executor::block_on(async {
        let mut items = Vec::new();
        while let Some(item) = response.body.next().await {
            items.push(item);
        }
        items
    })
}

fn assert_clean_eof_failure(item: &Result<Vec<u8>, TransportError>) {
    let error = item.as_ref().expect_err("clean EOF must fail");
    assert_eq!(
        error.message,
        "WebSocket stream closed before response.completed"
    );
    assert_eq!(error.diagnostics.len(), 1);
    let diagnostic = &error.diagnostics[0];
    assert_eq!(diagnostic.kind, "provider_transport_failure");
    assert_eq!(diagnostic.details["configuredTransport"], "auto");
    assert_eq!(diagnostic.details["eventsEmitted"], true);
    assert_eq!(diagnostic.details["phase"], "after_message_stream_start");
    assert!(!diagnostic.details.contains_key("fallbackTransport"));
}

fn decoded_codex_http_request(request: &HttpRequest) -> serde_json::Value {
    let body = if request
        .headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value == "zstd")
    {
        zstd::stream::decode_all(request.body.as_slice()).expect("zstd Codex body")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).expect("Codex request JSON")
}

fn assert_send_cancellation_clears_continuation() {
    let first_input = serde_json::json!([{"role":"user","content":"first"}]);
    let second_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"}
    ]);
    let third_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"},
        {"role":"user","content":"third"}
    ]);
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::FramesThenPending(partial_message_frames()),
        WebSocketReply::Frames(vec![done_frame("resp_after_cancel", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    consume_send_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, first_input),
        CancellationToken::new(),
    );

    let cancellation = CancellationToken::new();
    let mut second = futures_executor::block_on(transport.execute(
        codex_request(StreamTransport::WebsocketCached, second_input),
        cancellation.clone(),
    ))
    .expect("Send cancellable WebSocket response");
    let first = futures_executor::block_on(second.body.next())
        .expect("prefetched semantic frame")
        .expect("semantic frame");
    assert!(first.starts_with(b"data: "));
    cancellation.cancel();
    drop(second);

    consume_send_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, third_input.clone()),
        CancellationToken::new(),
    );
    assert!(http.requests.lock().unwrap().is_empty());
    let requests = websocket.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_seed");
    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(third.get("previous_response_id").is_none());
    assert_eq!(third["input"], third_input);
}

fn assert_local_cancellation_clears_continuation() {
    let first_input = serde_json::json!([{"role":"user","content":"first"}]);
    let second_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"}
    ]);
    let third_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"},
        {"role":"user","content":"third"}
    ]);
    let http = Rc::new(LocalRecordingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::FramesThenPending(partial_message_frames()),
        WebSocketReply::Frames(vec![done_frame("resp_after_cancel", Vec::new())]),
    ]));
    let transport =
        LocalOpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    consume_local_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, first_input),
        CancellationToken::new(),
    );

    let cancellation = CancellationToken::new();
    let mut second = futures_executor::block_on(transport.execute(
        codex_request(StreamTransport::WebsocketCached, second_input),
        cancellation.clone(),
    ))
    .expect("local cancellable WebSocket response");
    let first = futures_executor::block_on(second.body.next())
        .expect("prefetched semantic frame")
        .expect("semantic frame");
    assert!(first.starts_with(b"data: "));
    cancellation.cancel();
    drop(second);

    consume_local_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, third_input.clone()),
        CancellationToken::new(),
    );
    assert!(http.requests.borrow().is_empty());
    let requests = websocket.requests.borrow();
    assert_eq!(requests.len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_seed");
    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(third.get("previous_response_id").is_none());
    assert_eq!(third["input"], third_input);
}

fn assert_send_semantic_error_clears_continuation(error_frame: Vec<u8>) {
    let first_input = serde_json::json!([{"role":"user","content":"first"}]);
    let second_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"}
    ]);
    let third_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"},
        {"role":"user","content":"third"}
    ]);
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::Frames(vec![error_frame]),
        WebSocketReply::Frames(vec![done_frame("resp_after_error", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    consume_send_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, first_input),
        CancellationToken::new(),
    );
    consume_send_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, second_input),
        CancellationToken::new(),
    );
    consume_send_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, third_input.clone()),
        CancellationToken::new(),
    );
    assert!(http.requests.lock().unwrap().is_empty());
    let requests = websocket.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_seed");
    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(third.get("previous_response_id").is_none());
    assert_eq!(third["input"], third_input);
}

fn assert_local_semantic_error_clears_continuation(error_frame: Vec<u8>) {
    let first_input = serde_json::json!([{"role":"user","content":"first"}]);
    let second_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"}
    ]);
    let third_input = serde_json::json!([
        {"role":"user","content":"first"},
        {"role":"user","content":"second"},
        {"role":"user","content":"third"}
    ]);
    let http = Rc::new(LocalRecordingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(vec![done_frame("resp_seed", Vec::new())]),
        WebSocketReply::Frames(vec![error_frame]),
        WebSocketReply::Frames(vec![done_frame("resp_after_error", Vec::new())]),
    ]));
    let transport =
        LocalOpenAiCodexResponsesTransport::with_websocket(http.clone(), websocket.clone());
    consume_local_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, first_input),
        CancellationToken::new(),
    );
    consume_local_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, second_input),
        CancellationToken::new(),
    );
    consume_local_response(
        &transport,
        codex_request(StreamTransport::WebsocketCached, third_input.clone()),
        CancellationToken::new(),
    );
    assert!(http.requests.borrow().is_empty());
    let requests = websocket.requests.borrow();
    assert_eq!(requests.len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_seed");
    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(third.get("previous_response_id").is_none());
    assert_eq!(third["input"], third_input);
}

fn send_retry_request() -> ResolvedApiRequest {
    let model = openai_codex_models().unwrap().remove(0);
    let endpoint = model.common.base_url.clone();
    let options = retry_options();
    let mut retry_policy = openai_codex_retry_policy();
    retry_policy.max_retries = 2;
    ResolvedApiRequest {
        model,
        context: Context::new(None),
        request_options: ApiRequestOptions::from(&options),
        options,
        full_options: None,
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: retry_auth_headers(),
        api_key: None,
        api: ApiId::new(OpenAiCodexResponses::API_ID),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy,
        timeout: None,
        retry_classifier: Arc::new(OpenAiCodexRetryClassifier::default()),
    }
}

fn local_retry_request() -> LocalResolvedApiRequest {
    let model = openai_codex_models().unwrap().remove(0);
    let endpoint = model.common.base_url.clone();
    let options = retry_options();
    let mut retry_policy = openai_codex_retry_policy();
    retry_policy.max_retries = 2;
    LocalResolvedApiRequest {
        model,
        context: Context::new(None),
        request_options: ApiRequestOptions::from(&options),
        options,
        full_options: None,
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: retry_auth_headers(),
        api_key: None,
        api: ApiId::new(OpenAiCodexResponses::API_ID),
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy,
        timeout: None,
        retry_classifier: Rc::new(LocalOpenAiCodexRetryClassifier::default()),
    }
}

fn retry_options() -> SimpleGenerationOptions {
    SimpleGenerationOptions {
        max_retries: Some(2),
        transport: Some(StreamTransport::Auto),
        ..Default::default()
    }
}

fn retry_auth_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer credential"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("account-123"),
    );
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("pi-ai-rs/0.1.0"),
    );
    headers
}

fn codex_request(transport: StreamTransport, input: serde_json::Value) -> HttpRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer hostile-overlay"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("hostile-account"),
    );
    headers.insert("originator", HeaderValue::from_static("hostile"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("hostile"));
    headers.insert("session-id", HeaderValue::from_static("session-1"));
    let mut auth_headers = HeaderMap::new();
    auth_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer credential"),
    );
    auth_headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("account-123"),
    );
    auth_headers.insert("originator", HeaderValue::from_static("pi"));
    auth_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("pi-ai-rs/0.1.0"),
    );
    HttpRequest {
        method: Method::POST,
        url: Url::parse("https://chatgpt.com/backend-api").unwrap(),
        headers,
        auth_headers,
        session_id: Some("session-1".into()),
        body: serde_json::to_vec(&serde_json::json!({
            "model":"gpt-5.4",
            "store":false,
            "stream":true,
            "input":input
        }))
        .unwrap(),
        timeout: Some(std::time::Duration::from_secs(2)),
        transport: Some(transport),
        websocket_connect_timeout: Some(std::time::Duration::from_secs(1)),
        attempt: 0,
    }
}

fn done_frame(response_id: &str, output: Vec<serde_json::Value>) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"response.done",
        "response":{
            "id":response_id,
            "model":"gpt-5.4",
            "status":"completed",
            "output":output
        }
    }))
    .unwrap()
}

fn output_item_done_frame(item: serde_json::Value) -> Vec<u8> {
    output_item_done_frame_at(0, item)
}

fn output_item_done_frame_at(output_index: u64, item: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"response.output_item.done",
        "output_index":output_index,
        "item":item
    }))
    .unwrap()
}

fn partial_message_frames() -> Vec<Vec<u8>> {
    [
        serde_json::json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{
                "type":"message",
                "id":"msg_partial",
                "role":"assistant",
                "status":"in_progress",
                "content":[]
            }
        }),
        serde_json::json!({
            "type":"response.output_text.delta",
            "output_index":0,
            "delta":"partial"
        }),
    ]
    .into_iter()
    .map(|event| serde_json::to_vec(&event).unwrap())
    .collect()
}

fn semantic_error_frames_without_codes() -> Vec<Vec<u8>> {
    vec![
        serde_json::to_vec(&serde_json::json!({
            "type":"error",
            "message":"top-level failure"
        }))
        .unwrap(),
        serde_json::to_vec(&serde_json::json!({
            "type":"response.failed",
            "response":{"error":{"message":"response failure"}}
        }))
        .unwrap(),
    ]
}

fn function_call_stream_frames(terminal_arguments: Option<&str>) -> Vec<Vec<u8>> {
    let added = serde_json::json!({
        "type":"response.output_item.added",
        "output_index":0,
        "item":{
            "type":"function_call",
            "id":"fc_streamed",
            "call_id":"call_streamed",
            "name":"read_file",
            "arguments":""
        }
    });
    let delta = serde_json::json!({
        "type":"response.function_call_arguments.delta",
        "output_index":0,
        "delta":"{\"path\":\"README.md\"}"
    });
    let mut terminal = serde_json::json!({
        "type":"function_call",
        "id":"fc_streamed",
        "call_id":"call_streamed",
        "name":"read_file",
        "status":"completed"
    });
    if let Some(arguments) = terminal_arguments {
        terminal["arguments"] = serde_json::Value::String(arguments.into());
    }
    vec![
        serde_json::to_vec(&added).unwrap(),
        serde_json::to_vec(&delta).unwrap(),
        output_item_done_frame_at(0, terminal),
    ]
}

fn custom_tool_stream_frames() -> Vec<Vec<u8>> {
    [
        serde_json::json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{
                "type":"custom_tool_call",
                "id":"ctc_streamed",
                "call_id":"call_custom",
                "name":"query",
                "input":""
            }
        }),
        serde_json::json!({
            "type":"response.custom_tool_call_input.delta",
            "output_index":0,
            "delta":"hel"
        }),
        serde_json::json!({
            "type":"response.custom_tool_call_input.delta",
            "output_index":0,
            "delta":"lo"
        }),
        serde_json::json!({
            "type":"response.output_item.done",
            "output_index":0,
            "item":{
                "type":"custom_tool_call",
                "id":"ctc_streamed",
                "call_id":"call_custom",
                "name":"query",
                "status":"completed"
            }
        }),
    ]
    .into_iter()
    .map(|event| serde_json::to_vec(&event).unwrap())
    .collect()
}

fn cached_tool_output(call: &serde_json::Value) -> serde_json::Value {
    let output_type = match call["type"].as_str() {
        Some("custom_tool_call") => "custom_tool_call_output",
        _ => "function_call_output",
    };
    serde_json::json!({
        "type":output_type,
        "call_id":call["call_id"],
        "output":"contents"
    })
}

fn assert_send_cached_tool_continuation(
    mut first_frames: Vec<Vec<u8>>,
    canonical_call: serde_json::Value,
) {
    first_frames.push(done_frame("resp_streamed", Vec::new()));
    let http = Arc::new(RecordingHttp::default());
    let websocket = Arc::new(RecordingWebSocket::new([
        WebSocketReply::Frames(first_frames),
        WebSocketReply::Frames(vec![done_frame("resp_next", Vec::new())]),
    ]));
    let transport = OpenAiCodexResponsesTransport::with_websocket(http, websocket.clone());
    let output = cached_tool_output(&canonical_call);
    let inputs = [
        serde_json::json!([{"role":"user","content":"hello"}]),
        serde_json::json!([
            {"role":"user","content":"hello"},
            canonical_call,
            output.clone()
        ]),
    ];
    for input in inputs {
        let mut response = futures_executor::block_on(transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let requests = websocket.requests.lock().unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_streamed");
    assert_eq!(second["input"], serde_json::json!([output]));
}

fn assert_local_cached_tool_continuation(
    mut first_frames: Vec<Vec<u8>>,
    canonical_call: serde_json::Value,
) {
    first_frames.push(done_frame("resp_streamed", Vec::new()));
    let http = Rc::new(LocalRecordingHttp::default());
    let websocket = Rc::new(LocalRecordingWebSocket::new([
        WebSocketReply::Frames(first_frames),
        WebSocketReply::Frames(vec![done_frame("resp_next", Vec::new())]),
    ]));
    let transport = LocalOpenAiCodexResponsesTransport::with_websocket(http, websocket.clone());
    let output = cached_tool_output(&canonical_call);
    let inputs = [
        serde_json::json!([{"role":"user","content":"hello"}]),
        serde_json::json!([
            {"role":"user","content":"hello"},
            canonical_call,
            output.clone()
        ]),
    ];
    for input in inputs {
        let mut response = futures_executor::block_on(transport.execute(
            codex_request(StreamTransport::WebsocketCached, input),
            CancellationToken::new(),
        ))
        .expect("local cached WebSocket response");
        futures_executor::block_on(async { while response.body.next().await.is_some() {} });
    }
    let requests = websocket.requests.borrow();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "resp_streamed");
    assert_eq!(second["input"], serde_json::json!([output]));
}

fn error_frame(code: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"error",
        "code":code,
        "message":code
    }))
    .unwrap()
}

fn nested_error_frame(code: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"error",
        "error":{"code":code,"message":"cached continuation missing"}
    }))
    .unwrap()
}

fn rate_limits_frame() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"codex.rate_limits",
        "plan_type":"plus",
        "rate_limits":{"allowed":true,"limit_reached":false}
    }))
    .unwrap()
}
