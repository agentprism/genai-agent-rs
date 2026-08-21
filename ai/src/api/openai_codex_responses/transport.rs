use super::{CodexErrorKind, CodexRunError};
use crate::event_stream::{AssistantMessageEvent, AssistantStreamSender};
use crate::types::{AbortSignal, ProviderBodyStream, ProviderHeaders};
use futures::SinkExt;
use futures::future::pending;
use futures::stream::{BoxStream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message, protocol::CloseFrame};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub(super) const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub(super) const OPENAI_BETA_SSE: &str = "responses=experimental";
pub(super) const OPENAI_BETA_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
pub(super) const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_WEBSOCKET_MAX_AGE: Duration = Duration::from_secs(55 * 60);

type CodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenAICodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: usize,
    pub last_delta_input_items: Option<usize>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

#[derive(Clone)]
struct Continuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct SocketHandle {
    socket: tokio::sync::Mutex<CodexSocket>,
    alive: AtomicBool,
    close_requested: Notify,
}

impl SocketHandle {
    fn new(socket: CodexSocket) -> Self {
        Self {
            socket: tokio::sync::Mutex::new(socket),
            alive: AtomicBool::new(true),
            close_requested: Notify::new(),
        }
    }

    fn request_close(&self) {
        self.alive.store(false, Ordering::Release);
        self.close_requested.notify_waiters();
    }
}

struct CachedConnection {
    handle: Arc<SocketHandle>,
    busy: AtomicBool,
    created_at: Mutex<Instant>,
    idle_generation: AtomicU64,
    continuation: Mutex<Option<Continuation>>,
}

type AccountConnections = HashMap<String, Arc<CachedConnection>>;

fn websocket_cache() -> &'static Mutex<HashMap<String, AccountConnections>> {
    static CACHE: OnceLock<Mutex<HashMap<String, AccountConnections>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn debug_stats() -> &'static Mutex<HashMap<String, OpenAICodexWebSocketDebugStats>> {
    static STATS: OnceLock<Mutex<HashMap<String, OpenAICodexWebSocketDebugStats>>> =
        OnceLock::new();
    STATS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn fallback_sessions() -> &'static Mutex<HashSet<String>> {
    static SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn with_stats(session_id: &str, update: impl FnOnce(&mut OpenAICodexWebSocketDebugStats)) {
    let mut stats = lock(debug_stats());
    update(stats.entry(session_id.to_owned()).or_default());
}

pub(super) fn get_debug_stats(session_id: &str) -> Option<OpenAICodexWebSocketDebugStats> {
    lock(debug_stats()).get(session_id).cloned()
}

pub(super) fn reset_debug_stats(session_id: Option<&str>) {
    match session_id {
        Some(session_id) if !session_id.is_empty() => {
            lock(debug_stats()).remove(session_id);
            lock(fallback_sessions()).remove(session_id);
        }
        Some(_) | None => {
            lock(debug_stats()).clear();
            lock(fallback_sessions()).clear();
        }
    }
}

pub(super) fn fallback_active(session_id: Option<&str>) -> bool {
    session_id.is_some_and(|session_id| lock(fallback_sessions()).contains(session_id))
}

pub(super) fn record_sse_fallback(session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    let active = lock(fallback_sessions()).contains(session_id);
    with_stats(session_id, |stats| {
        stats.sse_fallbacks += 1;
        stats.websocket_fallback_active = Some(active);
    });
}

pub(super) fn record_websocket_failure(session_id: Option<&str>, error: &CodexRunError) {
    let Some(session_id) = session_id else {
        return;
    };
    lock(fallback_sessions()).insert(session_id.to_owned());
    with_stats(session_id, |stats| {
        stats.websocket_failures += 1;
        stats.last_websocket_error = Some(error.message.clone());
        stats.websocket_fallback_active = Some(true);
    });
}

pub(super) fn close_sessions(session_id: Option<&str>) {
    let entries = {
        let mut cache = lock(websocket_cache());
        match session_id {
            Some(session_id) if !session_id.is_empty() => cache
                .remove(session_id)
                .into_iter()
                .flat_map(|entries| entries.into_values())
                .collect::<Vec<_>>(),
            Some(_) | None => cache
                .drain()
                .flat_map(|(_, entries)| entries.into_values())
                .collect::<Vec<_>>(),
        }
    };
    for entry in entries {
        entry.handle.request_close();
        spawn_close(entry.handle.clone(), 1000, "debug_close");
    }
}

fn spawn_close(handle: Arc<SocketHandle>, code: u16, reason: &'static str) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            close_socket(&handle, code, reason).await;
        });
    }
}

async fn close_socket(handle: &SocketHandle, code: u16, reason: &str) {
    handle.alive.store(false, Ordering::Release);
    let mut socket = handle.socket.lock().await;
    let _ = socket
        .close(Some(CloseFrame {
            code: code.into(),
            reason: reason.to_owned().into(),
        }))
        .await;
}

pub(super) fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

pub(super) fn resolve_codex_websocket_url(base_url: &str) -> Result<String, CodexRunError> {
    let mut url = url::Url::parse(&resolve_codex_url(base_url)).map_err(CodexRunError::display)?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|()| CodexRunError::new("Invalid Codex WebSocket URL"))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|()| CodexRunError::new("Invalid Codex WebSocket URL"))?,
        _ => {}
    }
    Ok(url.into())
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), CodexRunError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(CodexRunError::display)?;
    let value = HeaderValue::from_str(value).map_err(CodexRunError::display)?;
    headers.insert(name, value);
    Ok(())
}

fn remove_header(headers: &mut HeaderMap, name: &str) {
    if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
        headers.remove(name);
    }
}

fn base_headers(
    model_headers: Option<&IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    user_agent: &str,
) -> Result<HeaderMap, CodexRunError> {
    let mut headers = HeaderMap::new();
    for (name, value) in model_headers.into_iter().flatten() {
        insert_header(&mut headers, name, value)?;
    }
    for (name, value) in additional_headers.into_iter().flatten() {
        if let Some(value) = value {
            insert_header(&mut headers, name, value)?;
        } else {
            remove_header(&mut headers, name);
        }
    }
    insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
    insert_header(&mut headers, "chatgpt-account-id", account_id)?;
    insert_header(&mut headers, "originator", "pi")?;
    insert_header(&mut headers, "user-agent", user_agent)?;
    Ok(headers)
}

pub(super) fn build_sse_headers(
    model_headers: Option<&IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    user_agent: &str,
    session_id: Option<&str>,
) -> Result<HeaderMap, CodexRunError> {
    let mut headers = base_headers(
        model_headers,
        additional_headers,
        account_id,
        token,
        user_agent,
    )?;
    insert_header(&mut headers, "openai-beta", OPENAI_BETA_SSE)?;
    insert_header(&mut headers, "accept", "text/event-stream")?;
    insert_header(&mut headers, "content-type", "application/json")?;
    if let Some(session_id) = session_id {
        insert_header(&mut headers, "session-id", session_id)?;
        insert_header(&mut headers, "x-client-request-id", session_id)?;
    }
    Ok(headers)
}

pub(super) fn build_websocket_headers(
    model_headers: Option<&IndexMap<String, String>>,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    user_agent: &str,
    request_id: &str,
) -> Result<HeaderMap, CodexRunError> {
    let mut headers = base_headers(
        model_headers,
        additional_headers,
        account_id,
        token,
        user_agent,
    )?;
    remove_header(&mut headers, "accept");
    remove_header(&mut headers, "content-type");
    remove_header(&mut headers, "openai-beta");
    // pi `headersToRecord()` lowercases this key before `delete wsHeaders["OpenAI-Beta"]`,
    // so the case-sensitive record deletion misses and the beta reaches the upgrade request
    // (`openai-codex-responses.ts:1050-1059,1634-1648`).
    insert_header(&mut headers, "openai-beta", OPENAI_BETA_WEBSOCKETS)?;
    insert_header(&mut headers, "x-client-request-id", request_id)?;
    insert_header(&mut headers, "session-id", request_id)?;
    Ok(headers)
}

pub(super) fn headers_to_record(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub(super) fn websocket_request_id(session_id: Option<&str>) -> String {
    session_id
        .filter(|session_id| !session_id.is_empty())
        .map_or_else(request_id, str::to_owned)
}

pub(super) struct AcquiredWebSocket {
    handle: Arc<SocketHandle>,
    entry: Option<Arc<CachedConnection>>,
    session_id: Option<String>,
    account_id: String,
    reused: bool,
}

impl AcquiredWebSocket {
    pub(super) fn has_cached_entry(&self) -> bool {
        self.entry.is_some()
    }

    pub(super) fn reused(&self) -> bool {
        self.reused
    }
}

async fn connect_websocket(
    url: &str,
    headers: &HeaderMap,
    signal: Option<Arc<dyn AbortSignal>>,
    connect_timeout_ms: Option<u64>,
) -> Result<Arc<SocketHandle>, CodexRunError> {
    let mut request = url
        .into_client_request()
        .map_err(CodexRunError::transport_display)?;
    for (name, value) in headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    let connect = tokio_tungstenite::connect_async(request);
    let result = if let Some(timeout_ms) = connect_timeout_ms.filter(|timeout| *timeout > 0) {
        tokio::select! {
            _ = wait_for_abort(signal.clone()) => return Err(CodexRunError::aborted("Request was aborted")),
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), connect) => {
                result.map_err(|_| CodexRunError::transport(format!("WebSocket connect timeout after {timeout_ms}ms")))?
            }
        }
    } else {
        tokio::select! {
            _ = wait_for_abort(signal) => return Err(CodexRunError::aborted("Request was aborted")),
            result = connect => result,
        }
    };
    let (socket, _) = result.map_err(CodexRunError::transport_display)?;
    Ok(Arc::new(SocketHandle::new(socket)))
}

pub(super) async fn acquire_websocket(
    url: &str,
    headers: &HeaderMap,
    session_id: Option<&str>,
    account_id: &str,
    signal: Option<Arc<dyn AbortSignal>>,
    connect_timeout_ms: Option<u64>,
) -> Result<AcquiredWebSocket, CodexRunError> {
    let connect_timeout_ms =
        Some(connect_timeout_ms.unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS));
    let Some(session_id) = session_id else {
        let handle = connect_websocket(url, headers, signal, connect_timeout_ms).await?;
        return Ok(AcquiredWebSocket {
            handle,
            entry: None,
            session_id: None,
            account_id: account_id.to_owned(),
            reused: false,
        });
    };

    let mut expired = None;
    let mut stale = None;
    let cached = {
        let mut cache = lock(websocket_cache());
        let mut remove_session = false;
        let cached = if let Some(account_entries) = cache.get_mut(session_id) {
            let cached = account_entries.get(account_id).cloned();
            if let Some(entry) = cached.as_ref() {
                if !entry.busy.load(Ordering::Acquire)
                    && lock(&entry.created_at).elapsed() >= SESSION_WEBSOCKET_MAX_AGE
                {
                    expired = account_entries.remove(account_id);
                    remove_session = account_entries.is_empty();
                    None
                } else if entry.handle.alive.load(Ordering::Acquire)
                    && entry
                        .busy
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    Some(entry.clone())
                } else {
                    if !entry.busy.load(Ordering::Acquire)
                        && !entry.handle.alive.load(Ordering::Acquire)
                    {
                        stale = account_entries.remove(account_id);
                        remove_session = account_entries.is_empty();
                    }
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if remove_session {
            cache.remove(session_id);
        }
        cached
    };
    if let Some(expired) = expired {
        expired.handle.request_close();
        spawn_close(expired.handle.clone(), 1000, "connection_age_limit");
    }
    if let Some(stale) = stale {
        stale.handle.request_close();
        spawn_close(stale.handle.clone(), 1000, "done");
    }
    if let Some(entry) = cached {
        return Ok(AcquiredWebSocket {
            handle: entry.handle.clone(),
            entry: Some(entry),
            session_id: Some(session_id.to_owned()),
            account_id: account_id.to_owned(),
            reused: true,
        });
    }

    let handle = connect_websocket(url, headers, signal, connect_timeout_ms).await?;
    let entry = Arc::new(CachedConnection {
        handle: handle.clone(),
        busy: AtomicBool::new(true),
        created_at: Mutex::new(Instant::now()),
        idle_generation: AtomicU64::new(0),
        continuation: Mutex::new(None),
    });
    let inserted = {
        let mut cache = lock(websocket_cache());
        let entries = cache.entry(session_id.to_owned()).or_default();
        if entries
            .get(account_id)
            .is_some_and(|existing| existing.busy.load(Ordering::Acquire))
        {
            false
        } else {
            entries.insert(account_id.to_owned(), entry.clone());
            true
        }
    };
    Ok(AcquiredWebSocket {
        handle,
        entry: inserted.then_some(entry),
        session_id: inserted.then(|| session_id.to_owned()),
        account_id: account_id.to_owned(),
        reused: false,
    })
}

pub(super) async fn send_websocket_frame(
    acquired: &AcquiredWebSocket,
    frame: String,
    signal: Option<Arc<dyn AbortSignal>>,
) -> Result<(), CodexRunError> {
    let send = async {
        acquired
            .handle
            .socket
            .lock()
            .await
            .send(Message::Text(frame.into()))
            .await
            .map_err(CodexRunError::transport_display)
    };
    tokio::select! {
        _ = wait_for_abort(signal) => Err(CodexRunError::aborted("Request was aborted")),
        result = send => result,
    }
}

fn remove_cached(acquired: &AcquiredWebSocket) {
    let (Some(session_id), Some(entry)) = (&acquired.session_id, &acquired.entry) else {
        return;
    };
    let mut cache = lock(websocket_cache());
    if let Some(entries) = cache.get_mut(session_id) {
        if entries
            .get(&acquired.account_id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&acquired.account_id);
        }
        if entries.is_empty() {
            cache.remove(session_id);
        }
    }
}

pub(super) fn release_websocket(acquired: &AcquiredWebSocket, keep: bool) {
    let Some(entry) = acquired.entry.as_ref() else {
        acquired.handle.request_close();
        spawn_close(acquired.handle.clone(), 1000, "done");
        return;
    };
    if !keep || !entry.handle.alive.load(Ordering::Acquire) {
        remove_cached(acquired);
        entry.handle.request_close();
        spawn_close(entry.handle.clone(), 1000, "done");
        return;
    }
    entry.busy.store(false, Ordering::Release);
    let generation = entry.idle_generation.fetch_add(1, Ordering::AcqRel) + 1;
    let deadline = tokio::time::Instant::now() + SESSION_WEBSOCKET_CACHE_TTL;
    let session_id = acquired.session_id.clone().expect("cached session");
    let account_id = acquired.account_id.clone();
    let entry = entry.clone();
    tokio::spawn(async move {
        tokio::time::sleep_until(deadline).await;
        if entry.busy.load(Ordering::Acquire)
            || entry.idle_generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        let removed = {
            let mut cache = lock(websocket_cache());
            let mut removed = false;
            if let Some(entries) = cache.get_mut(&session_id) {
                if entries
                    .get(&account_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    entries.remove(&account_id);
                    removed = true;
                }
                if entries.is_empty() {
                    cache.remove(&session_id);
                }
            }
            removed
        };
        if removed {
            entry.handle.request_close();
            close_socket(&entry.handle, 1000, "idle_timeout").await;
        }
    });
}

fn body_input(body: &Value) -> Vec<Value> {
    body.get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn bodies_match_except_input(left: &Value, right: &Value) -> bool {
    fn stripped(value: &Value) -> Value {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("input");
            object.remove("previous_response_id");
        }
        value
    }
    serde_json::to_string(&stripped(left)).ok() == serde_json::to_string(&stripped(right)).ok()
}

pub(super) fn cached_request_body(acquired: &AcquiredWebSocket, body: &Value) -> Value {
    let Some(entry) = acquired.entry.as_ref() else {
        return body.clone();
    };
    let continuation = lock(&entry.continuation).clone();
    let Some(continuation) = continuation else {
        return body.clone();
    };
    if !bodies_match_except_input(body, &continuation.last_request_body) {
        *lock(&entry.continuation) = None;
        return body.clone();
    }
    let current = body_input(body);
    let mut baseline = body_input(&continuation.last_request_body);
    baseline.extend(continuation.last_response_items);
    if current.len() < baseline.len() || current[..baseline.len()] != baseline {
        *lock(&entry.continuation) = None;
        return body.clone();
    }
    let mut request = body.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert(
            "previous_response_id".to_owned(),
            Value::String(continuation.last_response_id),
        );
        object.insert(
            "input".to_owned(),
            Value::Array(current[baseline.len()..].to_vec()),
        );
    }
    request
}

pub(super) fn set_continuation(
    acquired: &AcquiredWebSocket,
    request_body: Value,
    response_id: String,
    response_items: Vec<Value>,
) {
    if let Some(entry) = acquired.entry.as_ref() {
        *lock(&entry.continuation) = Some(Continuation {
            last_request_body: request_body,
            last_response_id: response_id,
            last_response_items: response_items,
        });
    }
}

pub(super) fn clear_continuation(acquired: &AcquiredWebSocket) {
    if let Some(entry) = acquired.entry.as_ref() {
        *lock(&entry.continuation) = None;
    }
}

pub(super) fn record_websocket_request(
    session_id: Option<&str>,
    acquired: &AcquiredWebSocket,
    cached_context: bool,
    body: &Value,
) {
    let Some(session_id) = session_id else {
        return;
    };
    let input_items = body_input(body).len();
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|response_id| !response_id.is_empty())
        .map(str::to_owned);
    with_stats(session_id, |stats| {
        stats.requests += 1;
        if acquired.reused() {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if cached_context {
            stats.cached_context_requests += 1;
        }
        if body.get("store").and_then(Value::as_bool) == Some(true) {
            stats.store_true_requests += 1;
        }
        stats.last_input_items = input_items;
        if let Some(previous_response_id) = previous_response_id {
            stats.delta_requests += 1;
            stats.last_delta_input_items = Some(input_items);
            stats.last_previous_response_id = Some(previous_response_id);
        } else {
            stats.full_context_requests += 1;
            stats.last_delta_input_items = None;
            stats.last_previous_response_id = None;
        }
    });
}

async fn wait_for_abort(signal: Option<Arc<dyn AbortSignal>>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => pending::<()>().await,
    }
}

async fn next_websocket_message(
    handle: Arc<SocketHandle>,
    signal: Option<Arc<dyn AbortSignal>>,
    idle_timeout_ms: Option<u64>,
) -> Result<Option<Message>, CodexRunError> {
    let receive = async {
        let mut socket = handle.socket.lock().await;
        socket.next().await.transpose()
    };
    let receive = async {
        if let Some(timeout_ms) = idle_timeout_ms.filter(|timeout| *timeout > 0) {
            tokio::time::timeout(Duration::from_millis(timeout_ms), receive)
                .await
                .map_err(|_| {
                    CodexRunError::transport(format!("WebSocket idle timeout after {timeout_ms}ms"))
                })?
                .map_err(CodexRunError::transport_display)
        } else {
            receive.await.map_err(CodexRunError::transport_display)
        }
    };
    tokio::select! {
        _ = wait_for_abort(signal) => Err(CodexRunError::aborted("Request was aborted")),
        _ = handle.close_requested.notified() => Err(CodexRunError::transport("WebSocket closed")),
        result = receive => result,
    }
}

fn close_message(frame: Option<CloseFrame>) -> String {
    let Some(frame) = frame else {
        return "WebSocket closed".to_owned();
    };
    let code = u16::from(frame.code);
    let reason = if frame.reason.is_empty() && code == 1009 {
        "message too big".to_owned()
    } else {
        frame.reason.to_string()
    };
    if reason.is_empty() {
        format!("WebSocket closed {code}")
    } else {
        format!("WebSocket closed {code} {reason}")
    }
}

fn raw_websocket_stream(
    handle: Arc<SocketHandle>,
    signal: Option<Arc<dyn AbortSignal>>,
    idle_timeout_ms: Option<u64>,
) -> BoxStream<'static, Result<Value, CodexRunError>> {
    struct State {
        handle: Arc<SocketHandle>,
        signal: Option<Arc<dyn AbortSignal>>,
        idle_timeout_ms: Option<u64>,
        done: bool,
        saw_completion: bool,
    }
    futures::stream::unfold(
        State {
            handle,
            signal,
            idle_timeout_ms,
            done: false,
            saw_completion: false,
        },
        |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                let message = match next_websocket_message(
                    state.handle.clone(),
                    state.signal.clone(),
                    state.idle_timeout_ms,
                )
                .await
                {
                    Ok(message) => message,
                    Err(error) => {
                        state.handle.alive.store(false, Ordering::Release);
                        state.done = true;
                        return Some((Err(error), state));
                    }
                };
                let text = match message {
                    Some(Message::Text(text)) => text.to_string(),
                    Some(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                    Some(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                    Some(Message::Close(frame)) => {
                        state.handle.alive.store(false, Ordering::Release);
                        state.done = true;
                        if state.saw_completion {
                            return None;
                        }
                        let code = frame.as_ref().map(|frame| u16::from(frame.code));
                        return Some((
                            Err(CodexRunError::websocket_close(close_message(frame), code)),
                            state,
                        ));
                    }
                    None => {
                        state.handle.alive.store(false, Ordering::Release);
                        state.done = true;
                        if state.saw_completion {
                            return None;
                        }
                        return Some((
                            Err(CodexRunError::transport(
                                "WebSocket stream closed before response.completed",
                            )),
                            state,
                        ));
                    }
                };
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => {
                        let kind = value.get("type").and_then(Value::as_str);
                        if matches!(
                            kind,
                            Some("response.completed" | "response.done" | "response.incomplete")
                        ) {
                            state.saw_completion = true;
                        }
                        return Some((Ok(value), state));
                    }
                    Err(error) => {
                        state.done = true;
                        return Some((
                            Err(CodexRunError::protocol(format!(
                                "Invalid Codex WebSocket JSON: {error}"
                            ))),
                            state,
                        ));
                    }
                }
            }
        },
    )
    .boxed()
}

fn event_error(value: &Value) -> (Option<String>, Option<String>) {
    let nested = value.get("error");
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| {
            nested
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            nested
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);
    (code, message)
}

fn normalize_event(mut value: Value) -> Result<Option<(Value, bool)>, CodexRunError> {
    let Some(kind) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(None);
    };
    if kind == "error" {
        let (code, message) = event_error(&value);
        let detail = message
            .clone()
            .or_else(|| code.clone())
            .unwrap_or_else(|| value.to_string());
        return Err(CodexRunError::api(format!("Codex error: {detail}"), code));
    }
    if kind == "response.failed" {
        let error = value
            .get("response")
            .and_then(|response| response.get("error"));
        let code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("Codex response failed");
        return Err(CodexRunError::api(message, code));
    }
    let terminal = matches!(
        kind.as_str(),
        "response.done" | "response.completed" | "response.incomplete"
    );
    if terminal && let Some(object) = value.as_object_mut() {
        object.insert(
            "type".to_owned(),
            Value::String("response.completed".to_owned()),
        );
        if let Some(response) = object.get_mut("response").and_then(Value::as_object_mut) {
            if response
                .get("end_turn")
                .is_some_and(|end_turn| !end_turn.is_boolean() && !end_turn.is_null())
            {
                response.remove("end_turn");
            }
            let known = response
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| {
                    matches!(
                        status,
                        "completed"
                            | "incomplete"
                            | "failed"
                            | "cancelled"
                            | "queued"
                            | "in_progress"
                    )
                });
            if !known {
                response.remove("status");
            }
        }
    }
    Ok(Some((value, terminal)))
}

fn normalize_stream(
    raw: BoxStream<'static, Result<Value, CodexRunError>>,
    sender: Option<AssistantStreamSender>,
    start_emitted: Arc<AtomicBool>,
    attempt_started: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<CodexRunError>>>,
) -> BoxStream<'static, Result<Value, CodexRunError>> {
    struct State {
        raw: BoxStream<'static, Result<Value, CodexRunError>>,
        sender: Option<AssistantStreamSender>,
        start_emitted: Arc<AtomicBool>,
        attempt_started: Arc<AtomicBool>,
        failure: Arc<Mutex<Option<CodexRunError>>>,
        done: bool,
    }
    futures::stream::unfold(
        State {
            raw,
            sender,
            start_emitted,
            attempt_started,
            failure,
            done: false,
        },
        |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                let item = state.raw.next().await?;
                let normalized = match item.and_then(normalize_event) {
                    Ok(Some(value)) => value,
                    Ok(None) => continue,
                    Err(error) => {
                        *lock(&state.failure) = Some(error.clone());
                        state.done = true;
                        return Some((Err(error), state));
                    }
                };
                state.attempt_started.store(true, Ordering::Release);
                if let Some(sender) = state.sender.as_ref()
                    && !state.start_emitted.swap(true, Ordering::AcqRel)
                    && sender.send(AssistantMessageEvent::Start).is_err()
                {
                    return None;
                }
                state.done = normalized.1;
                return Some((Ok(normalized.0), state));
            }
        },
    )
    .boxed()
}

pub(super) fn websocket_event_stream(
    acquired: &AcquiredWebSocket,
    signal: Option<Arc<dyn AbortSignal>>,
    idle_timeout_ms: Option<u64>,
    sender: AssistantStreamSender,
    start_emitted: Arc<AtomicBool>,
    attempt_started: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<CodexRunError>>>,
) -> BoxStream<'static, Result<Value, CodexRunError>> {
    normalize_stream(
        raw_websocket_stream(acquired.handle.clone(), signal, idle_timeout_ms),
        Some(sender),
        start_emitted,
        attempt_started,
        failure,
    )
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(index) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let frame = self.buffer.drain(..index + 2).collect::<Vec<_>>();
            let frame = String::from_utf8_lossy(&frame[..index]);
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                frames.push(data.to_owned());
            }
        }
        frames
    }
}

pub(super) fn sse_event_stream(
    body: ProviderBodyStream,
    signal: Option<Arc<dyn AbortSignal>>,
) -> BoxStream<'static, Result<Value, CodexRunError>> {
    struct State {
        body: ProviderBodyStream,
        signal: Option<Arc<dyn AbortSignal>>,
        decoder: SseDecoder,
        pending: std::collections::VecDeque<String>,
        done: bool,
    }
    let raw = futures::stream::unfold(
        State {
            body,
            signal,
            decoder: SseDecoder::default(),
            pending: Default::default(),
            done: false,
        },
        |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                if let Some(frame) = state.pending.pop_front() {
                    let value = serde_json::from_str(&frame).map_err(|error| {
                        CodexRunError::protocol(format!("Invalid Codex SSE JSON: {error}"))
                    });
                    return Some((value, state));
                }
                let chunk = tokio::select! {
                    _ = wait_for_abort(state.signal.clone()) => {
                        state.done = true;
                        return Some((Err(CodexRunError::aborted("Request was aborted")), state));
                    }
                    chunk = state.body.next() => chunk,
                };
                match chunk {
                    Some(Ok(bytes)) => state.pending.extend(state.decoder.push(&bytes)),
                    Some(Err(error)) => {
                        state.done = true;
                        return Some((Err(CodexRunError::transport(error)), state));
                    }
                    None => {
                        return None;
                    }
                }
            }
        },
    )
    .boxed();
    normalize_stream(
        raw,
        None,
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
        Arc::new(Mutex::new(None)),
    )
}

pub(super) fn take_failure(failure: &Arc<Mutex<Option<CodexRunError>>>) -> Option<CodexRunError> {
    lock(failure).take()
}

pub(super) fn response_create_frame(body: &Value) -> Result<String, CodexRunError> {
    let mut frame = Map::new();
    frame.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    match body {
        Value::Object(object) => frame.extend(object.clone()),
        Value::Array(items) => frame.extend(
            items
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone())),
        ),
        Value::String(value) => frame.extend(
            value
                .chars()
                .enumerate()
                .map(|(index, value)| (index.to_string(), Value::String(value.to_string()))),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    serde_json::to_string(&Value::Object(frame)).map_err(CodexRunError::display)
}

pub(super) fn classify_failure(error: &CodexRunError) -> &CodexErrorKind {
    &error.kind
}

#[cfg(test)]
pub(super) fn age_cached_session(session_id: &str, age: Duration) {
    let entries = lock(websocket_cache())
        .get(session_id)
        .map(|entries| entries.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for entry in entries {
        *lock(&entry.created_at) = Instant::now() - age;
    }
}
