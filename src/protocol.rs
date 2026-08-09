//! Codex wire protocol constants, URL/header derivation, and error-body parsing.
//!
//! Every constant and derivation below is ported from pi-ai's
//! `packages/ai/src/api/openai-codex-responses.ts`; line citations are inline.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::token::CodexToken;

/// Default Codex backend base URL (`DEFAULT_CODEX_BASE_URL`, openai-codex-responses.ts:59).
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// JWT claim namespace wrapping `chatgpt_account_id` (`JWT_CLAIM_PATH`, :60).
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// `OpenAI-Beta` value used on the **SSE** transport
/// (`headers.set("OpenAI-Beta", "responses=experimental")`, :1622).
pub const OPENAI_BETA_SSE: &str = "responses=experimental";

/// `OpenAI-Beta` value the WS path *builds* (`OPENAI_BETA_RESPONSES_WEBSOCKETS`, :827,
/// applied :1646). NOTE: it is then stripped from the actual WS upgrade request by
/// `connectWebSocket` (`delete wsHeaders["OpenAI-Beta"]`, :1050), so it never goes on
/// the wire. Kept here for documentation/fidelity only.
pub const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

/// Default `originator` header (openai-codex-responses.ts:1608; matches
/// rust-genai-auth's `DEFAULT_ORIGINATOR`).
pub const DEFAULT_ORIGINATOR: &str = "pi";

/// Resolve the Codex Responses **SSE** endpoint from a base URL.
///
/// Port of `resolveCodexUrl` (openai-codex-responses.ts:638-644): trim trailing
/// slashes, then append `/codex/responses` unless the base already ends in
/// `/codex/responses` or `/codex`.
pub fn resolve_sse_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Resolve the Codex Responses **WebSocket** endpoint from a base URL.
///
/// Port of `resolveCodexWebSocketUrl` (openai-codex-responses.ts:646-651): take
/// [`resolve_sse_url`] and swap the scheme (`https`→`wss`, `http`→`ws`).
pub fn resolve_ws_url(base_url: &str) -> String {
    let http = resolve_sse_url(base_url);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http
    }
}

/// Insert a header, silently skipping values that are not valid header content.
fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// Shared base headers for both transports (`buildBaseCodexHeaders`, :1592-1612):
/// `Authorization: Bearer …`, `chatgpt-account-id`, `originator`, `User-Agent`.
fn base_headers(token: &CodexToken, originator: &str, user_agent: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    set_header(
        &mut headers,
        "authorization",
        &format!("Bearer {}", token.bearer),
    );
    set_header(&mut headers, "chatgpt-account-id", &token.account_id);
    set_header(&mut headers, "originator", originator);
    set_header(&mut headers, "user-agent", user_agent);
    headers
}

/// Build the **SSE** request headers (`buildSSEHeaders`, :1614-1632).
///
/// Base + `OpenAI-Beta: responses=experimental`, `accept: text/event-stream`,
/// `content-type: application/json`. When a session/cache key is present pi also
/// sets `session-id` and `x-client-request-id` (:1626-1629).
pub fn build_sse_headers(
    token: &CodexToken,
    originator: &str,
    user_agent: &str,
    session_id: Option<&str>,
) -> HeaderMap {
    let mut headers = base_headers(token, originator, user_agent);
    set_header(&mut headers, "openai-beta", OPENAI_BETA_SSE);
    set_header(&mut headers, "accept", "text/event-stream");
    set_header(&mut headers, "content-type", "application/json");
    if let Some(session_id) = session_id {
        set_header(&mut headers, "session-id", session_id);
        set_header(&mut headers, "x-client-request-id", session_id);
    }
    headers
}

/// Build the **WebSocket** upgrade headers (`buildWebSocketHeaders`, :1634-1650,
/// then `connectWebSocket`'s `delete wsHeaders["OpenAI-Beta"]`, :1050).
///
/// Base (no `accept`, no `content-type`) + `x-client-request-id: requestId` +
/// `session-id: requestId`. Crucially, `OpenAI-Beta` is **not** sent on the WS
/// handshake — pi builds it then strips it before connecting.
pub fn build_ws_headers(
    token: &CodexToken,
    originator: &str,
    user_agent: &str,
    request_id: &str,
) -> HeaderMap {
    let mut headers = base_headers(token, originator, user_agent);
    set_header(&mut headers, "x-client-request-id", request_id);
    set_header(&mut headers, "session-id", request_id);
    headers
}

/// Generate a WebSocket request/correlation id.
///
/// pi uses `codexSessionId || uuidv7()` (:288). Without a session we mint a
/// fresh, monotonic-ish 128-bit id (wall-clock ms prefix + a process counter +
/// entropy), hex-encoded. It is a correlation id, not a security token, so
/// non-cryptographic uniqueness is sufficient.
pub fn gen_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Mix a little address entropy so parallel processes are unlikely to collide.
    let entropy = (&COUNTER as *const AtomicU64 as u64) ^ now_ms.rotate_left(21);
    let mut x = now_ms ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ entropy;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    format!("{now_ms:012x}{seq:08x}{x:016x}")
}

/// Parse a non-2xx SSE error body into a user-facing message.
///
/// Port of `parseErrorResponse` (openai-codex-responses.ts:1548-1573): surface a
/// friendly ChatGPT usage-limit message for usage/rate-limit codes or a 429,
/// otherwise the provider `error.message`, otherwise the raw body / status text.
pub fn parse_error_response(status: u16, body: &str, now_ms: i64) -> String {
    let mut message = if body.is_empty() {
        format!("Request failed (status {status})")
    } else {
        body.to_string()
    };

    if let Ok(parsed) = serde_json::from_str::<Value>(body)
        && let Some(err) = parsed.get("error")
    {
        let code = err
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| err.get("type").and_then(Value::as_str))
            .unwrap_or("");
        let is_usage_limit = code_is_usage_limit(code) || status == 429;
        let friendly = if is_usage_limit {
            let plan = err
                .get("plan_type")
                .and_then(Value::as_str)
                .map(|p| format!(" ({} plan)", p.to_lowercase()))
                .unwrap_or_default();
            let when = err
                .get("resets_at")
                .and_then(Value::as_f64)
                .map(|resets_at| {
                    let mins = (((resets_at * 1000.0) - now_ms as f64) / 60000.0)
                        .round()
                        .max(0.0) as i64;
                    format!(" Try again in ~{mins} min.")
                })
                .unwrap_or_default();
            Some(
                format!("You have hit your ChatGPT usage limit{plan}.{when}")
                    .trim()
                    .to_string(),
            )
        } else {
            None
        };
        // pi throws `friendlyMessage || message` (openai-codex-responses.ts:447),
        // so the friendly usage/rate-limit text wins over a provider `err.message`.
        // `friendly` is `Some` only for the usage-limit case, so other errors still
        // surface `err.message`, then the raw body.
        message = friendly
            .or_else(|| {
                err.get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(message);
    }

    message
}

/// `/usage_limit_reached|usage_not_included|rate_limit_exceeded/i` (:1560).
fn code_is_usage_limit(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    code.contains("usage_limit_reached")
        || code.contains("usage_not_included")
        || code.contains("rate_limit_exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_url_default_and_variants() {
        assert_eq!(
            resolve_sse_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // trailing slash trimmed
        assert_eq!(
            resolve_sse_url("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        // already ends with /codex -> +/responses
        assert_eq!(
            resolve_sse_url("http://127.0.0.1:9/backend-api/codex"),
            "http://127.0.0.1:9/backend-api/codex/responses"
        );
        // already fully qualified -> unchanged
        assert_eq!(
            resolve_sse_url("http://127.0.0.1:9/codex/responses"),
            "http://127.0.0.1:9/codex/responses"
        );
        // empty falls back to the default constant
        assert_eq!(
            resolve_sse_url("   "),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn ws_url_swaps_scheme() {
        assert_eq!(
            resolve_ws_url("https://chatgpt.com/backend-api"),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_ws_url("http://127.0.0.1:9"),
            "ws://127.0.0.1:9/codex/responses"
        );
    }

    #[test]
    fn sse_headers_carry_auth_account_and_content_type() {
        let token = CodexToken::new("tok-abc", "acct_123");
        let headers = build_sse_headers(&token, "pi", "ua/1", Some("sess-1"));
        assert_eq!(headers["authorization"], "Bearer tok-abc");
        assert_eq!(headers["chatgpt-account-id"], "acct_123");
        assert_eq!(headers["originator"], "pi");
        assert_eq!(headers["openai-beta"], OPENAI_BETA_SSE);
        assert_eq!(headers["accept"], "text/event-stream");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["session-id"], "sess-1");
        assert_eq!(headers["x-client-request-id"], "sess-1");
    }

    #[test]
    fn ws_headers_omit_openai_beta_and_content_type() {
        let token = CodexToken::new("tok-abc", "acct_123");
        let headers = build_ws_headers(&token, "pi", "ua/1", "req-9");
        assert_eq!(headers["authorization"], "Bearer tok-abc");
        assert_eq!(headers["chatgpt-account-id"], "acct_123");
        assert_eq!(headers["x-client-request-id"], "req-9");
        assert_eq!(headers["session-id"], "req-9");
        // OpenAI-Beta / accept / content-type must not be on the WS handshake.
        assert!(!headers.contains_key("openai-beta"));
        assert!(!headers.contains_key("accept"));
        assert!(!headers.contains_key("content-type"));
    }

    #[test]
    fn error_body_usage_limit_is_friendly() {
        let body =
            r#"{"error":{"code":"usage_limit_reached","plan_type":"Plus","resets_at":2000}}"#;
        // now = 1000s in ms; resets_at 2000s -> ~16-17 min.
        let msg = parse_error_response(429, body, 1_000_000);
        assert!(msg.starts_with("You have hit your ChatGPT usage limit (plus plan)."));
        assert!(msg.contains("Try again in ~"));
    }

    #[test]
    fn error_body_prefers_message() {
        let body = r#"{"error":{"code":"bad_request","message":"nope"}}"#;
        assert_eq!(parse_error_response(400, body, 0), "nope");
    }

    #[test]
    fn usage_limit_friendly_wins_over_message() {
        // L4: pi throws `friendlyMessage || message`, so a usage-limit friendly
        // message must win even when the provider also sends `error.message`.
        let body = r#"{"error":{"code":"usage_limit_reached","plan_type":"Pro","message":"raw provider text"}}"#;
        let msg = parse_error_response(429, body, 0);
        assert!(
            msg.starts_with("You have hit your ChatGPT usage limit (pro plan)."),
            "unexpected message: {msg}"
        );
        assert!(!msg.contains("raw provider text"));
    }

    #[test]
    fn error_body_falls_back_to_raw() {
        assert_eq!(parse_error_response(500, "boom", 0), "boom");
        assert_eq!(
            parse_error_response(500, "", 0),
            "Request failed (status 500)"
        );
    }

    #[test]
    fn request_ids_are_unique() {
        let a = gen_request_id();
        let b = gen_request_id();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
