//! Optional loopback redirect-capture server for the browser login flow
//! (feature = `loopback`).
//!
//! This is the application-side helper that pi implements with `http.createServer`
//! (openai-codex.ts:320-394): it listens on the loopback interface, waits for the
//! OAuth provider to redirect the browser to `/{path}?code=...&state=...`,
//! validates `state`, serves a small "you can close this window" page, and
//! returns the authorization code.
//!
//! It is deliberately kept out of the core so the OAuth logic stays fully
//! headless-testable; the core [`crate::CodexAuth`] never opens a browser or
//! binds a socket.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{Error, Result};

/// Environment variable that overrides the loopback bind host (pi parity:
/// `PI_OAUTH_CALLBACK_HOST`, openai-codex.ts:45).
pub const CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";

/// Loopback server configuration.
#[derive(Debug, Clone)]
pub struct LoopbackConfig {
    /// Bind host (default: `$PI_OAUTH_CALLBACK_HOST` or `127.0.0.1`).
    pub host: String,
    /// Bind port (default `1455`, matching [`crate::codex::REDIRECT_URI`]).
    pub port: u16,
    /// Expected callback path (default `/auth/callback`).
    pub path: String,
    /// How long to wait for the redirect before giving up.
    pub timeout: Duration,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            host: std::env::var(CALLBACK_HOST_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: 1455,
            path: "/auth/callback".to_string(),
            timeout: Duration::from_secs(5 * 60),
        }
    }
}

/// Bind the loopback server and wait for a single valid OAuth redirect.
///
/// Returns the captured authorization `code`. Requests to other paths get 404,
/// a `state` mismatch or a missing code gets 400, and the awaited valid request
/// gets a 200 success page. Times out per [`LoopbackConfig::timeout`].
pub async fn capture_redirect(config: &LoopbackConfig, expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
    match tokio::time::timeout(
        config.timeout,
        accept_loop(&listener, config, expected_state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Error::DeviceTimeout),
    }
}

/// Like [`capture_redirect`] but binds an ephemeral port (`port = 0`) and hands
/// the bound [`std::net::SocketAddr`] back via `on_bound` before awaiting. Useful
/// for tests and for callers that discover the port at runtime.
pub async fn capture_redirect_with<F>(
    config: &LoopbackConfig,
    expected_state: &str,
    on_bound: F,
) -> Result<String>
where
    F: FnOnce(std::net::SocketAddr),
{
    let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
    on_bound(listener.local_addr()?);
    match tokio::time::timeout(
        config.timeout,
        accept_loop(&listener, config, expected_state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Error::DeviceTimeout),
    }
}

async fn accept_loop(
    listener: &TcpListener,
    config: &LoopbackConfig,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let request_head = match read_request_head(&mut socket).await {
            Ok(head) => head,
            Err(_) => continue, // ignore malformed connections (e.g. port probes)
        };

        let target = request_head.lines().next().and_then(request_target);
        let Some(target) = target else {
            respond(&mut socket, 400, "Bad Request", ERROR_HTML).await?;
            continue;
        };

        // Parse against a dummy base to read path + query.
        let url = match reqwest::Url::parse(&format!("http://localhost{target}")) {
            Ok(url) => url,
            Err(_) => {
                respond(&mut socket, 400, "Bad Request", ERROR_HTML).await?;
                continue;
            }
        };

        if url.path() != config.path {
            respond(&mut socket, 404, "Not Found", ERROR_HTML).await?;
            continue;
        }

        let state = query_first(&url, "state");
        if state.as_deref() != Some(expected_state) {
            respond(&mut socket, 400, "Bad Request", STATE_MISMATCH_HTML).await?;
            continue;
        }

        match query_first(&url, "code") {
            Some(code) if !code.is_empty() => {
                respond(&mut socket, 200, "OK", SUCCESS_HTML).await?;
                return Ok(code);
            }
            _ => {
                respond(&mut socket, 400, "Bad Request", MISSING_CODE_HTML).await?;
            }
        }
    }
}

async fn read_request_head(socket: &mut tokio::net::TcpStream) -> Result<String> {
    // Read the whole request head (up to `\r\n\r\n`). Draining the headers before
    // responding avoids a TCP RST (and a spurious ConnectionReset on the client)
    // when we then close the socket; OAuth callbacks are header-only GETs.
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 8192 {
            break; // guard against unbounded input
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Extract the request target from an HTTP request line: `GET /path?q HTTP/1.1`.
fn request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

fn query_first(url: &reqwest::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

const SUCCESS_HTML: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title><body style=\"font-family:system-ui;padding:2rem\"><h1>Authentication complete</h1><p>You can close this window and return to your terminal.</p></body>";
const ERROR_HTML: &str = "<!doctype html><meta charset=utf-8><title>Error</title><body style=\"font-family:system-ui;padding:2rem\"><h1>Callback error</h1><p>This route did not match the expected OAuth callback.</p></body>";
const STATE_MISMATCH_HTML: &str = "<!doctype html><meta charset=utf-8><title>Error</title><body style=\"font-family:system-ui;padding:2rem\"><h1>State mismatch</h1><p>The OAuth state did not match. Please retry login.</p></body>";
const MISSING_CODE_HTML: &str = "<!doctype html><meta charset=utf-8><title>Error</title><body style=\"font-family:system-ui;padding:2rem\"><h1>Missing authorization code</h1></body>";
