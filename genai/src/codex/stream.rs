//! [`CodexStreamFn`]: a [`StreamFn`] that talks to the ChatGPT-subscription Codex
//! backend (`chatgpt.com/backend-api`), the Rust equivalent of pi-ai's
//! `openai-codex-responses.ts` `stream` export.
//!
//! # Transport (WebSocket with SSE fallback)
//!
//! Mirrors pi's transport model (openai-codex-responses.ts:300-489):
//!
//! - [`Transport::Sse`] — SSE only.
//! - [`Transport::Websocket`] / [`Transport::WebsocketCached`] / [`Transport::Auto`]
//!   — try the WebSocket transport, **falling back to SSE** when the WebSocket
//!   fails at the *transport* level before any assistant event is committed
//!   (handshake/upgrade failure, connect timeout, an unexpected close, an I/O
//!   error before the first frame, or an in-band
//!   `{"type":"error","code":"websocket_connection_limit_reached"}` frame — pi's
//!   :349, :701-703). Every OTHER *application* error delivered as a Codex `error`
//!   / `response.failed` frame is terminal and does **not** fall back (pi's
//!   `isCodexNonTransportError`, :697-699). A transport failure *after* the first
//!   frame is committed becomes a terminal in-band error (pi re-throws once
//!   `websocketStarted`, :373).
//!
//! `Auto` mirrors pi's default (`options?.transport || "auto"`, :300): WebSocket
//! with SSE fallback. The per-request [`StreamRequest::transport`] takes
//! precedence when it is non-[`Transport::Auto`]; otherwise the instance's
//! configured preference applies.
//!
//! ## Deliberate simplifications vs pi (documented)
//!
//! - The cross-request **WebSocket connection cache / `previous_response_id`
//!   continuation** (pi's `websocket-cached`, :841-1035, 1400-1542) is not ported;
//!   each request opens a fresh single-shot WebSocket. `WebsocketCached` therefore
//!   behaves like `Websocket` here.
//! - The pre-start **connection-limit / previous-response-not-found retry loop**
//!   (:308-379) is not ported as a *retry*. Instead, a pre-commit in-band
//!   connection-limit `error` frame (`websocket_connection_limit_reached`) falls
//!   back to SSE **once** (pi's :349-378). Every OTHER in-band `error` /
//!   `response.failed` frame is terminal and never falls back (pi's
//!   `isCodexNonTransportError`, :697-699); a connection-limit frame arriving
//!   *after* commit is likewise terminal (pi re-throws once started, :373).
//! - Request-body **zstd compression** is not sent (see [`crate::codex::request`]).
//!
//! # Never-throw contract
//!
//! Per [`StreamFn`], setup failures (token resolution, request send, non-2xx
//! handshake), stream failures (protocol/transport), and cancellation are all
//! reported **in-band** as a terminal [`AssistantMessageEvent::Error`] — never a
//! panic or a returned `Err`. Cancellation yields [`StopReason::Aborted`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use genai::{
    AssistantAccumulator, AssistantMessageEvent, AssistantMessageEventStream, StreamFn,
    StreamRequest, Transport,
};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::codex::events::{CodexEventMapper, MappedItem, SseDecoder};
use crate::codex::protocol::{
    DEFAULT_CODEX_BASE_URL, DEFAULT_ORIGINATOR, build_sse_headers, build_ws_headers,
    gen_request_id, parse_error_response, resolve_sse_url, resolve_ws_url,
};
use crate::codex::request::{BodyConfig, build_request_body, build_ws_create_frame};
use crate::codex::token::TokenSource;

/// A [`StreamFn`] for the ChatGPT-plan Codex backend.
///
/// Construct with [`CodexStreamFn::new`] (a [`TokenSource`]) and, optionally,
/// override the base URL, transport preference, HTTP client, originator,
/// user-agent, and request-body knobs. Clone is cheap (all fields are shared or
/// small).
#[derive(Clone)]
pub struct CodexStreamFn {
    token_source: Arc<dyn TokenSource>,
    base_url: String,
    transport: Transport,
    http: reqwest::Client,
    originator: String,
    user_agent: String,
    body_config: BodyConfig,
    ws_connect_timeout: Duration,
}

impl std::fmt::Debug for CodexStreamFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexStreamFn")
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .field("originator", &self.originator)
            .field("user_agent", &self.user_agent)
            .field("body_config", &self.body_config)
            .field("ws_connect_timeout", &self.ws_connect_timeout)
            .finish_non_exhaustive()
    }
}

/// Default `User-Agent`, matching pi's shape `pi (<platform> <release>; <arch>)`
/// (openai-codex-responses.ts:1609, `pi (${_os.platform()} ${_os.release()}; ${_os.arch()})`).
///
/// The live ChatGPT backend can reject a non-pi UA, so the default reproduces
/// pi's exactly: Node platform names (`linux`/`darwin`/`win32`) and Node arch
/// names (`x64`/`arm64`), with the OS release read best-effort. Override with
/// [`CodexStreamFn::with_user_agent`].
fn default_user_agent() -> String {
    format!("pi ({} {}; {})", node_platform(), os_release(), node_arch())
}

/// Map Rust's [`std::env::consts::OS`] to Node's `os.platform()` names.
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        // linux (and other unix names) already match Node's platform strings.
        other => other,
    }
}

/// Map Rust's [`std::env::consts::ARCH`] to Node's `os.arch()` names.
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        // arm / riscv64 / ppc64 / s390x / … already match Node's arch strings.
        other => other,
    }
}

/// Best-effort OS release string (Node's `os.release()`). On Linux this is the
/// kernel release from `/proc/sys/kernel/osrelease`; elsewhere a sensible
/// constant is used (reading it portably would require `unsafe`/libc `uname`,
/// which the crate forbids). Never panics; falls back to `"unknown"`.
fn os_release() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|release| release.trim().to_string())
            .filter(|release| !release.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        "unknown".to_string()
    }
}

impl CodexStreamFn {
    /// Build a stream function from a [`TokenSource`], defaulting the base URL to
    /// [`DEFAULT_CODEX_BASE_URL`], the transport to [`Transport::Auto`], and the
    /// originator to `pi`.
    pub fn new(token_source: Arc<dyn TokenSource>) -> Self {
        Self {
            token_source,
            base_url: DEFAULT_CODEX_BASE_URL.to_string(),
            transport: Transport::Auto,
            http: reqwest::Client::new(),
            originator: DEFAULT_ORIGINATOR.to_string(),
            user_agent: default_user_agent(),
            body_config: BodyConfig::default(),
            ws_connect_timeout: Duration::from_secs(15),
        }
    }

    /// Override the Codex backend base URL (default [`DEFAULT_CODEX_BASE_URL`]).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the instance transport preference (applied when the request's transport
    /// advisory is [`Transport::Auto`]).
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Use a caller-provided `reqwest::Client` (proxies, timeouts, custom TLS…).
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Override the `originator` header (default `pi`).
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = originator.into();
        self
    }

    /// Override the `User-Agent` header.
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Replace the request-body configuration ([`BodyConfig`]).
    pub fn with_body_config(mut self, body_config: BodyConfig) -> Self {
        self.body_config = body_config;
        self
    }

    /// Set the `prompt_cache_key` (also used as the SSE `session-id` /
    /// `x-client-request-id`, mirroring pi's session id).
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.body_config.prompt_cache_key = Some(key.into());
        self
    }

    /// Set the `reasoning.summary` value used when a reasoning effort is requested.
    pub fn with_reasoning_summary(mut self, summary: impl Into<String>) -> Self {
        self.body_config.reasoning_summary = summary.into();
        self
    }

    /// Set the WebSocket connect timeout (default 15s, pi's
    /// `DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS`). A timeout is treated as a
    /// pre-commit transport failure and falls back to SSE.
    pub fn with_ws_connect_timeout(mut self, timeout: Duration) -> Self {
        self.ws_connect_timeout = timeout;
        self
    }
}

/// Resolve a [`ModelSpec`] to a [`ModelIden`] for the assistant message.
///
/// A bare model name is tagged with [`AdapterKind::OpenAIResp`] because the Codex
/// backend speaks the OpenAI Responses protocol.
fn model_iden_for(model: &ModelSpec) -> ModelIden {
    match model {
        ModelSpec::Iden(iden) => iden.clone(),
        ModelSpec::Target(target) => target.model.clone(),
        ModelSpec::Name(name) => ModelIden::new(AdapterKind::OpenAIResp, name.clone()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The outcome of translating one raw event frame.
struct FrameOutcome {
    /// Assistant events to emit, in order.
    events: Vec<AssistantMessageEvent>,
    /// Whether a *content* (non-error) item was folded (used only by the
    /// WebSocket fallback decision).
    committed: bool,
    /// Set when this frame is a pre-commit transport failure (the WS
    /// connection-limit `error` frame). The caller decides between SSE fallback
    /// (pre-commit, WebSocket) and a terminal in-band error (after commit / SSE).
    transport_fail: Option<String>,
}

/// Translate one raw event JSON string, folding it into `acc`.
///
/// A [`MappedItem::TransportFail`] is **not** folded here: it is surfaced via
/// [`FrameOutcome::transport_fail`] so the caller can fall back or terminate. A
/// JSON parse failure becomes an in-band terminal error (a protocol error, which
/// pi does not fall back from). This function never yields; the caller emits the
/// returned events so the `yield` stays inside the `async_stream` block.
fn process_frame(
    mapper: &mut CodexEventMapper,
    acc: &mut AssistantAccumulator,
    frame_json: &str,
) -> FrameOutcome {
    let value: Value = match serde_json::from_str(frame_json) {
        Ok(value) => value,
        Err(error) => {
            return FrameOutcome {
                events: acc.fail(format!("Invalid Codex event JSON: {error}")),
                committed: false,
                transport_fail: None,
            };
        }
    };

    let mut events = Vec::new();
    let mut committed = false;
    let mut transport_fail = None;
    for item in mapper.map(&value) {
        match item {
            MappedItem::Stream(event) => {
                committed = true;
                events.extend(acc.fold(event));
            }
            MappedItem::Fail(message) => {
                events.extend(acc.fail(message));
            }
            MappedItem::TransportFail(message) => {
                transport_fail = Some(message);
                break;
            }
        }
        if acc.is_terminal() {
            break;
        }
    }
    FrameOutcome {
        events,
        committed,
        transport_fail,
    }
}

#[async_trait]
impl StreamFn for CodexStreamFn {
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream {
        let StreamRequest {
            model,
            context,
            options,
            transport: req_transport,
            cancel,
            ..
        } = request;

        let model_iden = model_iden_for(&model);
        let model_id = model_iden.model_name.as_str().to_string();
        // Per-request advisory wins when explicit; otherwise the instance default.
        let effective_transport = if req_transport == Transport::Auto {
            self.transport
        } else {
            req_transport
        };

        // Own everything the generator needs so the stream is `'static`.
        let token_source = self.token_source.clone();
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let originator = self.originator.clone();
        let user_agent = self.user_agent.clone();
        let body_config = self.body_config.clone();
        let ws_connect_timeout = self.ws_connect_timeout;

        let stream = async_stream::stream! {
            let mut acc = AssistantAccumulator::new(model_iden.clone());

            // -- 1. Fresh token (bearer + account id), cancellation-aware --------
            let token = tokio::select! {
                biased;
                _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                token = token_source.fetch() => token,
            };
            let token = match token {
                Ok(token) => token,
                Err(error) => {
                    for ev in acc.fail(format!("Codex token resolution failed: {error}")) {
                        yield ev;
                    }
                    return;
                }
            };

            // -- 2. Request body -------------------------------------------------
            let body = build_request_body(&model_id, &context, &options, &body_config);
            let body_json = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

            let want_ws = effective_transport != Transport::Sse;
            let mut do_sse = !want_ws;

            // -- 3a. WebSocket transport (with SSE fallback) ---------------------
            if want_ws {
                let ws_url = resolve_ws_url(&base_url);
                let request_id = gen_request_id();
                let ws_headers = build_ws_headers(&token, &originator, &user_agent, &request_id);

                let connect = async {
                    let mut req = ws_url
                        .into_client_request()
                        .map_err(|e| e.to_string())?;
                    {
                        let headers = req.headers_mut();
                        for (name, value) in ws_headers.iter() {
                            headers.insert(name.clone(), value.clone());
                        }
                    }
                    tokio::time::timeout(
                        ws_connect_timeout,
                        tokio_tungstenite::connect_async(req),
                    )
                    .await
                    .map_err(|_| "WebSocket connect timeout".to_string())?
                    .map_err(|e| e.to_string())
                };

                let connect_result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                    result = connect => result,
                };

                match connect_result {
                    Ok((mut ws, _response)) => {
                        // Send the `response.create` frame (uncompressed JSON).
                        let create_frame = build_ws_create_frame(&body);
                        let sent = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                            result = ws.send(Message::text(create_frame)) => result,
                        };

                        if sent.is_err() {
                            // Could not even send the request -> pre-commit -> fall back.
                            do_sse = true;
                        } else {
                            let mut mapper = CodexEventMapper::new();
                            let mut committed = false;
                            let mut ws_error: Option<String> = None;

                            loop {
                                let message = tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                                    message = ws.next() => message,
                                };

                                let frame = match message {
                                    Some(Ok(Message::Text(text))) => text.as_str().to_string(),
                                    Some(Ok(Message::Binary(bytes))) => {
                                        String::from_utf8_lossy(&bytes).into_owned()
                                    }
                                    Some(Ok(
                                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_),
                                    )) => continue,
                                    Some(Ok(Message::Close(_))) | None => break,
                                    Some(Err(error)) => {
                                        ws_error = Some(error.to_string());
                                        break;
                                    }
                                };

                                let outcome = process_frame(&mut mapper, &mut acc, &frame);
                                if outcome.committed {
                                    committed = true;
                                }
                                for ev in outcome.events {
                                    yield ev;
                                }
                                if let Some(message) = outcome.transport_fail {
                                    if committed {
                                        // In-band connection-limit after commit ->
                                        // terminal (pi re-throws once started, :373).
                                        for ev in acc.fail(message) {
                                            yield ev;
                                        }
                                        return;
                                    }
                                    // Pre-commit connection-limit -> fall back to SSE
                                    // (pi's :349-378). Breaking with `!committed`
                                    // makes the post-loop set `do_sse = true`; the SSE
                                    // branch then installs a fresh accumulator so no
                                    // duplicate `start` is emitted.
                                    break;
                                }
                                if acc.is_terminal() {
                                    return;
                                }
                            }

                            // Broke out without a terminal event (close / EOF / IO error).
                            if committed {
                                let message = ws_error.unwrap_or_else(|| {
                                    "Codex WebSocket closed before a terminal response event"
                                        .to_string()
                                });
                                for ev in acc.fail(message) {
                                    yield ev;
                                }
                                return;
                            }
                            // Nothing committed -> transport failure -> fall back to SSE.
                            do_sse = true;
                        }
                    }
                    // Handshake / upgrade / connect-timeout failure -> fall back to SSE.
                    Err(_error) => {
                        do_sse = true;
                    }
                }
            }

            // -- 3b. SSE transport (primary, or WebSocket fallback) --------------
            if do_sse {
                // A fresh accumulator: a non-committed WebSocket attempt emitted
                // nothing, so SSE starts clean (no duplicate `start`).
                acc = AssistantAccumulator::new(model_iden.clone());

                let sse_url = resolve_sse_url(&base_url);
                let sse_headers = build_sse_headers(
                    &token,
                    &originator,
                    &user_agent,
                    body_config.prompt_cache_key.as_deref(),
                );

                let send = http
                    .post(&sse_url)
                    .headers(sse_headers)
                    .body(body_json.clone())
                    .send();
                let response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                    response = send => response,
                };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        for ev in acc.fail(format!("Codex SSE request failed: {error}")) {
                            yield ev;
                        }
                        return;
                    }
                };

                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    let body_text = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                        text = response.text() => text.unwrap_or_default(),
                    };
                    let message = parse_error_response(status, &body_text, now_ms());
                    for ev in acc.fail(message) {
                        yield ev;
                    }
                    return;
                }

                let mut byte_stream = response.bytes_stream();
                let mut decoder = SseDecoder::new();
                let mut mapper = CodexEventMapper::new();

                loop {
                    let chunk = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => { for ev in acc.abort() { yield ev; } return; }
                        chunk = byte_stream.next() => chunk,
                    };
                    match chunk {
                        Some(Ok(bytes)) => {
                            for frame in decoder.push(&bytes) {
                                let outcome = process_frame(&mut mapper, &mut acc, &frame);
                                for ev in outcome.events {
                                    yield ev;
                                }
                                if let Some(message) = outcome.transport_fail {
                                    // Nothing left to fall back to on the SSE path:
                                    // treat the connection-limit frame as terminal.
                                    for ev in acc.fail(message) {
                                        yield ev;
                                    }
                                    return;
                                }
                                if acc.is_terminal() {
                                    return;
                                }
                            }
                        }
                        Some(Err(error)) => {
                            for ev in acc.fail(format!("Codex SSE read error: {error}")) {
                                yield ev;
                            }
                            return;
                        }
                        None => break,
                    }
                }

                // Stream ended without a terminal response event.
                if !acc.is_terminal() {
                    for ev in acc.fail(
                        "Codex SSE stream ended before a terminal response event",
                    ) {
                        yield ev;
                    }
                }
            }
        };

        AssistantMessageEventStream::from_stream(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_user_agent_matches_pi_shape() {
        // M3: default UA is `pi (<platform> <release>; <arch>)`.
        let ua = default_user_agent();
        assert!(ua.starts_with("pi ("), "unexpected UA: {ua}");
        assert!(ua.ends_with(')'), "unexpected UA: {ua}");
        // The Node-mapped arch is present.
        assert!(
            ua.contains(node_arch()),
            "UA {ua} missing arch {}",
            node_arch()
        );
        // The Node-mapped platform is present, and Rust's raw names are not leaked.
        assert!(ua.contains(node_platform()), "UA {ua} missing platform");
        assert!(!ua.contains("x86_64") && !ua.contains("aarch64"));
    }

    #[test]
    fn node_platform_and_arch_mappings() {
        // Sanity: the current target maps to a Node-style name (never the raw
        // Rust constant for the mapped cases).
        assert_ne!(node_platform(), "macos");
        assert_ne!(node_arch(), "x86_64");
        assert_ne!(node_arch(), "aarch64");
    }
}
