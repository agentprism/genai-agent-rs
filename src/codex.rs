//! ChatGPT Codex (OpenAI Codex / ChatGPT-subscription) OAuth flow.
//!
//! Faithful port of pi-ai's `packages/ai/src/auth/oauth/openai-codex.ts`. Every
//! endpoint, id, scope, grant type, and body field below is cited to its
//! `openai-codex.ts` line.
//!
//! All endpoints except the loopback `REDIRECT_URI` are derived from
//! [`CodexConfig::base_url`] exactly as pi derives them from `AUTH_BASE_URL`, so
//! tests can point the whole flow at a local mock server by overriding `base_url`.
//!
//! The application owns opening the browser and capturing the loopback redirect;
//! this module only *produces* the authorize URL and *consumes* the returned
//! code (an optional loopback helper lives behind the `loopback` feature).

use serde_json::Value;

use crate::credential::{now_unix_ms, OAuthCredential};
use crate::device_code::{poll_device_code, DevicePoll, DevicePollOptions};
use crate::error::{Error, Result};
use crate::jwt::extract_chatgpt_account_id;
use crate::pkce::Pkce;

// ---------------------------------------------------------------------------
// Constants — ported verbatim from openai-codex.ts (line citations inline).
// ---------------------------------------------------------------------------

/// OAuth client id (openai-codex.ts:26).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OAuth issuer base URL (openai-codex.ts:27).
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
/// Loopback redirect URI for the browser flow (openai-codex.ts:30).
/// NOTE: fixed loopback, *not* derived from `AUTH_BASE_URL`.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// Device-code overall timeout, 15 minutes (openai-codex.ts:35).
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
/// Login-method selector id: browser (openai-codex.ts:36).
pub const BROWSER_LOGIN_METHOD: &str = "browser";
/// Login-method selector id: device code (openai-codex.ts:37).
pub const DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
/// Requested OAuth scope (openai-codex.ts:38).
pub const SCOPE: &str = "openid profile email offline_access";
/// Default `originator` query param (openai-codex.ts:294 default arg).
pub const DEFAULT_ORIGINATOR: &str = "pi";
/// Provider id used as the credential-store key (pi providers/openai-codex.ts:9).
pub const PROVIDER_ID: &str = "openai-codex";

// Path segments appended to `base_url` (openai-codex.ts:28-34):
const AUTHORIZE_PATH: &str = "/oauth/authorize"; // openai-codex.ts:28
const TOKEN_PATH: &str = "/oauth/token"; // openai-codex.ts:29
const DEVICE_USER_CODE_PATH: &str = "/api/accounts/deviceauth/usercode"; // openai-codex.ts:31
const DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token"; // openai-codex.ts:32
const DEVICE_VERIFICATION_PATH: &str = "/codex/device"; // openai-codex.ts:33
const DEVICE_REDIRECT_PATH: &str = "/deviceauth/callback"; // openai-codex.ts:34

/// Configuration for the Codex OAuth flow.
///
/// Defaults reproduce pi's constants; override `base_url` in tests to target a
/// local mock server.
#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// OAuth issuer base URL (default [`AUTH_BASE_URL`]).
    pub base_url: String,
    /// OAuth client id (default [`CLIENT_ID`]).
    pub client_id: String,
    /// Browser-flow loopback redirect URI (default [`REDIRECT_URI`]).
    pub redirect_uri: String,
    /// Requested scope (default [`SCOPE`]).
    pub scope: String,
    /// `originator` query param (default [`DEFAULT_ORIGINATOR`]).
    pub originator: String,
    /// Device-code overall timeout in seconds (default [`DEVICE_CODE_TIMEOUT_SECONDS`]).
    pub device_code_timeout_seconds: u64,
    /// Require a `chatgpt_account_id` in the token (pi parity: errors if absent).
    pub require_account_id: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: AUTH_BASE_URL.to_string(),
            client_id: CLIENT_ID.to_string(),
            redirect_uri: REDIRECT_URI.to_string(),
            scope: SCOPE.to_string(),
            originator: DEFAULT_ORIGINATOR.to_string(),
            device_code_timeout_seconds: DEVICE_CODE_TIMEOUT_SECONDS,
            require_account_id: true,
        }
    }
}

impl CodexConfig {
    fn authorize_url(&self) -> String {
        format!("{}{}", self.base_url, AUTHORIZE_PATH)
    }
    fn token_url(&self) -> String {
        format!("{}{}", self.base_url, TOKEN_PATH)
    }
    fn device_user_code_url(&self) -> String {
        format!("{}{}", self.base_url, DEVICE_USER_CODE_PATH)
    }
    fn device_token_url(&self) -> String {
        format!("{}{}", self.base_url, DEVICE_TOKEN_PATH)
    }
    /// The end-user verification URI shown during device-code login.
    pub fn device_verification_uri(&self) -> String {
        format!("{}{}", self.base_url, DEVICE_VERIFICATION_PATH)
    }
    /// The redirect URI used when exchanging a device-flow authorization code.
    pub fn device_redirect_uri(&self) -> String {
        format!("{}{}", self.base_url, DEVICE_REDIRECT_PATH)
    }
}

/// A pending browser login: the authorize URL to open plus the PKCE verifier and
/// CSRF state to carry until the redirect returns.
///
/// `Debug` is hand-written and **redacts the `verifier`** (the PKCE secret used
/// at token exchange). `authorize_url` and `state` are non-secret and stay
/// visible. Note the `authorize_url` embeds the S256 `code_challenge` (public),
/// not the verifier.
#[derive(Clone)]
pub struct PendingBrowserLogin {
    /// The URL the application should open in a browser.
    pub authorize_url: String,
    /// CSRF `state` value that the redirect must echo back.
    pub state: String,
    /// PKCE code verifier used at token exchange (keep secret).
    pub verifier: String,
}

impl std::fmt::Debug for PendingBrowserLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingBrowserLogin")
            .field("authorize_url", &self.authorize_url)
            .field("state", &self.state)
            .field("verifier", &"<redacted>")
            .finish()
    }
}

/// A started device login: codes to show the user and the derived timing.
#[derive(Debug, Clone)]
pub struct DeviceLoginBegin {
    /// Opaque device-auth id echoed back on each poll.
    pub device_auth_id: String,
    /// The code the user enters at the verification URI.
    pub user_code: String,
    /// Server-provided poll interval in seconds.
    pub interval_seconds: f64,
    /// Where the user completes verification (openai-codex.ts:33).
    pub verification_uri: String,
    /// Overall device-flow timeout in seconds.
    pub expires_in_seconds: u64,
}

/// A successful device-token poll: the authorization code and server-issued PKCE
/// verifier used for the follow-up code exchange (openai-codex.ts:54-57,252-263).
///
/// `Debug` **redacts both fields**: the `authorization_code` and `code_verifier`
/// are exchangeable for bearer tokens.
#[derive(Clone)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

impl std::fmt::Debug for DeviceTokenSuccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceTokenSuccess")
            .field("authorization_code", &"<redacted>")
            .field("code_verifier", &"<redacted>")
            .finish()
    }
}

/// Normalized token payload before it becomes an [`OAuthCredential`].
///
/// `Debug` **redacts the secret `access`/`refresh` tokens**; the non-secret
/// `expires_at_ms` stays visible.
#[derive(Clone)]
struct OAuthToken {
    access: String,
    refresh: String,
    expires_at_ms: i64,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// The ChatGPT Codex OAuth client.
#[derive(Debug, Clone)]
pub struct CodexAuth {
    config: CodexConfig,
    http: reqwest::Client,
}

impl Default for CodexAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAuth {
    /// New client with default config and a default `reqwest::Client`.
    pub fn new() -> Self {
        Self::with_config(CodexConfig::default())
    }

    /// New client with a custom config and a default `reqwest::Client`.
    pub fn with_config(config: CodexConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// New client with a custom config and a caller-provided `reqwest::Client`.
    pub fn with_client(config: CodexConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// The active configuration.
    pub fn config(&self) -> &CodexConfig {
        &self.config
    }

    // -- Browser flow ------------------------------------------------------

    /// Begin the browser login: generate PKCE + state and build the authorize URL.
    ///
    /// Pure and offline (no network). Mirrors `createAuthorizationFlow`
    /// (openai-codex.ts:293-312). Query params, in order:
    /// `response_type=code`, `client_id`, `redirect_uri`, `scope`,
    /// `code_challenge`, `code_challenge_method=S256`, `state`,
    /// `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
    /// `originator`.
    pub fn begin_browser_login(&self) -> Result<PendingBrowserLogin> {
        let pkce = Pkce::generate()?;
        let state = random_state()?;

        let url = reqwest::Url::parse_with_params(
            &self.config.authorize_url(),
            &[
                ("response_type", "code"),
                ("client_id", self.config.client_id.as_str()),
                ("redirect_uri", self.config.redirect_uri.as_str()),
                ("scope", self.config.scope.as_str()),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", state.as_str()),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", self.config.originator.as_str()),
            ],
        )
        .map_err(|e| Error::Url(e.to_string()))?;

        Ok(PendingBrowserLogin {
            authorize_url: url.to_string(),
            state,
            verifier: pkce.verifier,
        })
    }

    /// Complete the browser login from the redirect result.
    ///
    /// `redirect_or_code` may be a bare authorization code, a full redirect URL,
    /// a `code#state` fragment, or a `code=...&state=...` query string; it is
    /// parsed like pi's `parseAuthorizationInput` (openai-codex.ts:73-101). If a
    /// `state` is present it must match `pending.state` (openai-codex.ts:485).
    /// The code is then exchanged using the loopback `redirect_uri`.
    pub async fn complete_browser_login(
        &self,
        pending: &PendingBrowserLogin,
        redirect_or_code: &str,
    ) -> Result<OAuthCredential> {
        let parsed = parse_authorization_input(redirect_or_code);
        if let Some(state) = &parsed.state {
            if state != &pending.state {
                return Err(Error::StateMismatch);
            }
        }
        let code = parsed
            .code
            .filter(|c| !c.is_empty())
            .ok_or(Error::MissingAuthorizationCode)?;
        self.exchange_authorization_code(&code, &pending.verifier, &self.config.redirect_uri)
            .await
    }

    /// Low-level authorization-code exchange (openai-codex.ts:149-169).
    ///
    /// `POST {base}/oauth/token`, `application/x-www-form-urlencoded`, body:
    /// `grant_type=authorization_code`, `client_id`, `code`, `code_verifier`,
    /// `redirect_uri`.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthCredential> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", self.config.client_id.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ];
        let response = self
            .http
            .post(self.config.token_url())
            .form(&form)
            .send()
            .await?;
        let token = read_token_response(response, "exchange").await?;
        self.credential_from_token(token)
    }

    // -- Device-code flow --------------------------------------------------

    /// Begin device-code login: request a user code (openai-codex.ts:191-233).
    ///
    /// `POST {base}/api/accounts/deviceauth/usercode`, JSON body `{ client_id }`.
    /// A 404 means device login is not enabled server-side.
    pub async fn begin_device_login(&self) -> Result<DeviceLoginBegin> {
        let response = self
            .http
            .post(self.config.device_user_code_url())
            .json(&serde_json::json!({ "client_id": self.config.client_id }))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 404 {
                return Err(Error::DeviceCodeNotEnabled);
            }
            let body = response.text().await.unwrap_or_default();
            return Err(Error::DeviceCodeRequestFailed {
                status: status.as_u16(),
                body,
            });
        }

        let value: Value = response.json().await?;
        let device_auth_id = value.get("device_auth_id").and_then(Value::as_str);
        let user_code = value.get("user_code").and_then(Value::as_str);
        let interval_seconds = parse_interval(value.get("interval"));

        // pi parity + fail-closed: pi rejects when `interval` is not a finite,
        // non-negative number (openai-codex.ts:218-226). This reproduces that
        // (`interval.is_finite() && interval >= 0.0`); additionally a missing /
        // unparseable `interval` (`parse_interval` -> `None`) is rejected here
        // as an `InvalidDeviceCodeResponse` rather than silently defaulting,
        // which is intentionally stricter but fail-closed.
        match (device_auth_id, user_code, interval_seconds) {
            (Some(device_auth_id), Some(user_code), Some(interval))
                if interval.is_finite() && interval >= 0.0 =>
            {
                Ok(DeviceLoginBegin {
                    device_auth_id: device_auth_id.to_string(),
                    user_code: user_code.to_string(),
                    interval_seconds: interval,
                    verification_uri: self.config.device_verification_uri(),
                    expires_in_seconds: self.config.device_code_timeout_seconds,
                })
            }
            _ => Err(Error::InvalidDeviceCodeResponse {
                body: value.to_string(),
            }),
        }
    }

    /// Poll the device-token endpoint until authorized, then exchange the code.
    ///
    /// Mirrors `pollOpenAICodexDeviceAuth` + the follow-up exchange
    /// (openai-codex.ts:235-291, 437-442). The exchange uses the device
    /// redirect URI (`{base}/deviceauth/callback`) and the server-issued PKCE
    /// verifier returned by the poll.
    pub async fn poll_device_login(&self, begin: &DeviceLoginBegin) -> Result<OAuthCredential> {
        let success = self.poll_device_token(begin).await?;
        self.exchange_authorization_code(
            &success.authorization_code,
            &success.code_verifier,
            &self.config.device_redirect_uri(),
        )
        .await
    }

    async fn poll_device_token(&self, begin: &DeviceLoginBegin) -> Result<DeviceTokenSuccess> {
        let options = DevicePollOptions {
            interval_seconds: Some(begin.interval_seconds),
            expires_in_seconds: Some(begin.expires_in_seconds as f64),
            wait_before_first_poll: false,
        };

        // Own everything the per-poll future needs so each future is `'static`.
        let http = self.http.clone();
        let url = self.config.device_token_url();
        let device_auth_id = begin.device_auth_id.clone();
        let user_code = begin.user_code.clone();

        poll_device_code(options, move || {
            let http = http.clone();
            let url = url.clone();
            let device_auth_id = device_auth_id.clone();
            let user_code = user_code.clone();
            async move {
                let response = http
                    .post(&url)
                    .json(&serde_json::json!({
                        "device_auth_id": device_auth_id,
                        "user_code": user_code,
                    }))
                    .send()
                    .await?;

                let status = response.status();
                if status.is_success() {
                    let value: Value = response.json().await?;
                    let authorization_code =
                        value.get("authorization_code").and_then(Value::as_str);
                    let code_verifier = value.get("code_verifier").and_then(Value::as_str);
                    return Ok(match (authorization_code, code_verifier) {
                        (Some(ac), Some(cv)) => DevicePoll::Complete(DeviceTokenSuccess {
                            authorization_code: ac.to_string(),
                            code_verifier: cv.to_string(),
                        }),
                        _ => DevicePoll::Failed {
                            message: format!(
                                "Invalid OpenAI Codex device auth token response: {value}"
                            ),
                        },
                    });
                }

                // 403 / 404 are treated as "pending" (openai-codex.ts:266-268).
                if status.as_u16() == 403 || status.as_u16() == 404 {
                    return Ok(DevicePoll::Pending);
                }

                let body = response.text().await.unwrap_or_default();
                match error_code_of(&body).as_deref() {
                    Some("deviceauth_authorization_pending") => Ok(DevicePoll::Pending),
                    Some("slow_down") => Ok(DevicePoll::SlowDown {
                        interval_seconds: None,
                    }),
                    _ => Ok(DevicePoll::Failed {
                        message: format!(
                            "OpenAI Codex device auth failed with status {}{}",
                            status.as_u16(),
                            if body.is_empty() {
                                String::new()
                            } else {
                                format!(": {body}")
                            }
                        ),
                    }),
                }
            }
        })
        .await
    }

    // -- Refresh -----------------------------------------------------------

    /// Refresh the access token (openai-codex.ts:171-189).
    ///
    /// `POST {base}/oauth/token`, `application/x-www-form-urlencoded`, body:
    /// `grant_type=refresh_token`, `refresh_token`, `client_id`. Returns a fresh
    /// credential (with a re-derived account id).
    pub async fn refresh(&self, credential: &OAuthCredential) -> Result<OAuthCredential> {
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .ok_or(Error::MissingRefreshToken)?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.as_str()),
        ];
        let response = self
            .http
            .post(self.config.token_url())
            .form(&form)
            .send()
            .await?;
        let token = read_token_response(response, "refresh").await?;
        self.credential_from_token(token)
    }

    // -- Helpers -----------------------------------------------------------

    fn credential_from_token(&self, token: OAuthToken) -> Result<OAuthCredential> {
        let account_id = extract_chatgpt_account_id(&token.access);
        if self.config.require_account_id && account_id.is_none() {
            return Err(Error::MissingAccountId);
        }
        Ok(OAuthCredential::new(
            token.access,
            Some(token.refresh),
            Some(token.expires_at_ms),
            account_id,
        ))
    }
}

/// Parse the token endpoint response (openai-codex.ts:126-147).
async fn read_token_response(
    response: reqwest::Response,
    operation: &'static str,
) -> Result<OAuthToken> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::TokenRequest {
            operation,
            status: status.as_u16(),
            body,
        });
    }

    let value: Value = response.json().await?;
    // Reject empty-string tokens the way pi's truthiness checks do
    // (openai-codex.ts:138 `!json.access_token` / `!json.refresh_token`): an
    // empty token is treated as a missing field, not a valid credential.
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let expires_in = value.get("expires_in").and_then(Value::as_f64);

    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthToken {
            access: access.to_string(),
            refresh: refresh.to_string(),
            // expires = Date.now() + expires_in * 1000 (openai-codex.ts:145),
            // hardened against hostile/degenerate `expires_in` — see
            // [`expires_at_ms_from`].
            expires_at_ms: expires_at_ms_from(now_unix_ms(), expires_in),
        }),
        _ => Err(Error::TokenResponseMissingFields {
            operation,
            body: value.to_string(),
        }),
    }
}

/// Compute the absolute expiry (`now + expires_in*1000`) without panicking or
/// wrapping on a hostile `expires_in`.
///
/// A raw `now_unix_ms() + (expires_in * 1000.0) as i64` can panic in debug
/// builds and wrap in release for values like `1e300` or `f64::INFINITY`. This
/// clamps and saturates instead, matching the `saturating_add` posture in
/// `credential.rs`:
/// - non-finite (`NaN`/`±∞`) and negative values collapse to `0` seconds, i.e.
///   an already-expired credential that forces a refresh (fail-closed);
/// - huge-but-finite values saturate to a far-future, non-negative expiry
///   (capped at `i64::MAX`).
fn expires_at_ms_from(now_ms: i64, expires_in: f64) -> i64 {
    let secs = if expires_in.is_finite() {
        expires_in.clamp(0.0, i64::MAX as f64 / 1000.0)
    } else {
        0.0
    };
    // Rust's float-to-int `as` cast saturates (no UB), and `saturating_add`
    // caps at `i64::MAX`, so the result is always a valid, non-negative epoch ms.
    now_ms.saturating_add((secs * 1000.0) as i64)
}

/// Parsed authorization input (code and optional state).
///
/// `Debug` **redacts the `code`** (a single-use authorization code exchangeable
/// for tokens); `state` (a CSRF nonce) stays visible. `PartialEq` still compares
/// the real values, so tests keep their exact-equality semantics.
#[derive(Default, PartialEq)]
struct ParsedAuth {
    code: Option<String>,
    state: Option<String>,
}

impl std::fmt::Debug for ParsedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedAuth")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("state", &self.state)
            .finish()
    }
}

/// Port of `parseAuthorizationInput` (openai-codex.ts:73-101).
///
/// INTENTIONAL DEVIATION (stricter, fail-closed): pi returns whatever `code` it
/// parses — including an empty string — and only later rejects a falsy code. This
/// crate additionally treats a parsed empty `code` as *no code*: the caller
/// [`CodexAuth::complete_browser_login`] applies `.filter(|c| !c.is_empty())`, so
/// an empty/whitespace `code=` fragment fails closed with
/// [`Error::MissingAuthorizationCode`] rather than posting a blank code to the
/// token endpoint. Parsing itself stays byte-for-byte compatible for real codes.
fn parse_authorization_input(input: &str) -> ParsedAuth {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuth::default();
    }

    // 1) Full URL.
    if let Ok(url) = reqwest::Url::parse(value) {
        return ParsedAuth {
            code: query_first(&url, "code"),
            state: query_first(&url, "state"),
        };
    }

    // 2) `code#state`.
    if value.contains('#') {
        let mut parts = value.split('#');
        let code = parts.next().map(str::to_string);
        let state = parts.next().map(str::to_string);
        return ParsedAuth { code, state };
    }

    // 3) `code=...&state=...` query fragment.
    if value.contains("code=") {
        if let Ok(url) = reqwest::Url::parse(&format!("http://localhost/?{value}")) {
            return ParsedAuth {
                code: query_first(&url, "code"),
                state: query_first(&url, "state"),
            };
        }
    }

    // 4) Bare code.
    ParsedAuth {
        code: Some(value.to_string()),
        state: None,
    }
}

fn query_first(url: &reqwest::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Parse the device-code `interval` (number, or numeric string) (openai-codex.ts:217).
fn parse_interval(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Extract an error code from a device-token error body (openai-codex.ts:271-276).
/// Handles both `{ "error": "code" }` and `{ "error": { "code": "code" } }`.
fn error_code_of(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("error")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(obj) => obj.get("code").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn random_state() -> Result<String> {
    // createState(): randomBytes(16).toString("hex") (openai-codex.ts:66-71).
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;
    Ok(hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_browser_login_builds_authorize_url() {
        let auth = CodexAuth::new();
        let pending = auth.begin_browser_login().unwrap();

        assert!(pending
            .authorize_url
            .starts_with("https://auth.openai.com/oauth/authorize?"));
        let url = reqwest::Url::parse(&pending.authorize_url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["originator"], DEFAULT_ORIGINATOR);
        assert_eq!(q["state"], pending.state);
        // The challenge in the URL must match the pending verifier.
        assert_eq!(q["code_challenge"], Pkce::challenge_for(&pending.verifier));
        // state = 16 random bytes hex = 32 chars.
        assert_eq!(pending.state.len(), 32);
    }

    #[test]
    fn parse_authorization_input_variants() {
        // Bare code.
        assert_eq!(
            parse_authorization_input("abc123"),
            ParsedAuth {
                code: Some("abc123".into()),
                state: None
            }
        );
        // Full redirect URL.
        assert_eq!(
            parse_authorization_input("http://localhost:1455/auth/callback?code=xyz&state=st"),
            ParsedAuth {
                code: Some("xyz".into()),
                state: Some("st".into())
            }
        );
        // code#state.
        assert_eq!(
            parse_authorization_input("thecode#thestate"),
            ParsedAuth {
                code: Some("thecode".into()),
                state: Some("thestate".into())
            }
        );
        // Query fragment.
        assert_eq!(
            parse_authorization_input("code=q1&state=q2"),
            ParsedAuth {
                code: Some("q1".into()),
                state: Some("q2".into())
            }
        );
        // Empty.
        assert_eq!(parse_authorization_input("   "), ParsedAuth::default());
    }

    #[test]
    fn parse_interval_accepts_number_and_string() {
        assert_eq!(parse_interval(Some(&serde_json::json!(5))), Some(5.0));
        assert_eq!(parse_interval(Some(&serde_json::json!("3"))), Some(3.0));
        assert_eq!(parse_interval(Some(&serde_json::json!(" 2 "))), Some(2.0));
        assert_eq!(parse_interval(Some(&serde_json::json!("nope"))), None);
        assert_eq!(parse_interval(None), None);
    }

    #[test]
    fn error_code_of_handles_both_shapes() {
        assert_eq!(
            error_code_of(r#"{"error":"slow_down"}"#).as_deref(),
            Some("slow_down")
        );
        assert_eq!(
            error_code_of(r#"{"error":{"code":"deviceauth_authorization_pending"}}"#).as_deref(),
            Some("deviceauth_authorization_pending")
        );
        assert_eq!(error_code_of("not json"), None);
    }

    #[test]
    fn hex_encode_matches_expected() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn expires_at_ms_is_overflow_safe() {
        let now = 1_700_000_000_000_i64;

        // Normal case: exact arithmetic.
        assert_eq!(expires_at_ms_from(now, 3600.0), now + 3_600_000);

        // Hostile huge (but finite) value must not panic and must saturate to a
        // far-future, non-negative expiry (capped at i64::MAX).
        let huge = expires_at_ms_from(now, 1e300);
        assert_eq!(huge, i64::MAX, "1e300 should saturate to i64::MAX");
        assert!(huge > now, "expiry must be far in the future");

        // Even at i64::MAX `now`, a positive expires_in saturates rather than wraps.
        assert_eq!(expires_at_ms_from(i64::MAX, 1e300), i64::MAX);
        assert_eq!(expires_at_ms_from(i64::MAX, 3600.0), i64::MAX);

        // Non-finite / negative collapse to "already expired" (fail-closed).
        assert_eq!(expires_at_ms_from(now, f64::INFINITY), now);
        assert_eq!(expires_at_ms_from(now, f64::NEG_INFINITY), now);
        assert_eq!(expires_at_ms_from(now, f64::NAN), now);
        assert_eq!(expires_at_ms_from(now, -5.0), now);
    }

    #[test]
    fn derived_endpoints_follow_base_url() {
        let config = CodexConfig {
            base_url: "http://127.0.0.1:9".to_string(),
            ..CodexConfig::default()
        };
        assert_eq!(config.token_url(), "http://127.0.0.1:9/oauth/token");
        assert_eq!(config.authorize_url(), "http://127.0.0.1:9/oauth/authorize");
        assert_eq!(
            config.device_user_code_url(),
            "http://127.0.0.1:9/api/accounts/deviceauth/usercode"
        );
        assert_eq!(
            config.device_token_url(),
            "http://127.0.0.1:9/api/accounts/deviceauth/token"
        );
        assert_eq!(
            config.device_verification_uri(),
            "http://127.0.0.1:9/codex/device"
        );
        assert_eq!(
            config.device_redirect_uri(),
            "http://127.0.0.1:9/deviceauth/callback"
        );
        // redirect_uri is fixed loopback, not derived from base_url.
        assert_eq!(config.redirect_uri, REDIRECT_URI);
    }
}
