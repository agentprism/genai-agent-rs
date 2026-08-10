//! Error and `Result` types for `genai::auth`.

use thiserror::Error;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// All failures produced by this crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying HTTP transport / request failure.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON (de)serialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Filesystem failure (credential store).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed URL while building an authorize/token endpoint.
    #[error("invalid url: {0}")]
    Url(String),

    /// The OS CSPRNG failed to produce bytes for the PKCE verifier / CSRF state.
    #[error("failed to generate secure random bytes: {0}")]
    Random(String),

    // -- Token endpoint (openai-codex.ts:126-147) --
    /// Token endpoint returned a non-2xx status.
    /// Mirrors `OpenAI Codex token {operation} failed ({status}): {body}`.
    ///
    /// REDACTION WARNING: `body` is the raw token-endpoint response and may
    /// contain sensitive data (error descriptions, echoed request parameters).
    /// Callers should redact or omit it before logging; it is included in this
    /// error's `Display` for diagnostics only.
    #[error("OpenAI Codex token {operation} failed ({status}): {body}")]
    TokenRequest {
        /// "exchange" or "refresh".
        operation: &'static str,
        /// HTTP status code.
        status: u16,
        /// Response body (best-effort). May contain response data — redact before logging.
        body: String,
    },

    /// Token endpoint returned 2xx but is missing `access_token` / `refresh_token`
    /// / `expires_in`. Mirrors `OpenAI Codex token {operation} response missing fields`.
    ///
    /// REDACTION WARNING: `body` is the raw JSON response and may include token
    /// material (e.g. a present-but-rejected `access_token`). Redact before logging.
    #[error("OpenAI Codex token {operation} response missing fields: {body}")]
    TokenResponseMissingFields {
        /// "exchange" or "refresh".
        operation: &'static str,
        /// The raw JSON that failed validation. May contain response data — redact before logging.
        body: String,
    },

    // -- Browser flow --
    /// The `state` returned by the redirect did not match the pending login.
    #[error("OAuth state mismatch")]
    StateMismatch,

    /// No authorization code could be parsed from the redirect / manual input.
    #[error("Missing authorization code")]
    MissingAuthorizationCode,

    // -- Account id (openai-codex.ts:405-407) --
    /// The access token JWT did not contain a `chatgpt_account_id` claim.
    #[error("Failed to extract accountId from token")]
    MissingAccountId,

    // -- Refresh --
    /// `refresh()` was called on a credential with no refresh token.
    #[error("credential has no refresh token")]
    MissingRefreshToken,

    // -- Device code flow (openai-codex.ts:200-208, device-code.ts) --
    /// The device-code endpoint returned 404 (feature not enabled server-side).
    #[error(
        "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."
    )]
    DeviceCodeNotEnabled,

    /// The device-code `usercode` request failed with a non-2xx (non-404) status.
    ///
    /// REDACTION WARNING: `body` is the raw response and may contain response
    /// data; redact before logging.
    #[error("OpenAI Codex device code request failed with status {status}: {body}")]
    DeviceCodeRequestFailed {
        /// HTTP status code.
        status: u16,
        /// Response body (best-effort). May contain response data — redact before logging.
        body: String,
    },

    /// The device-code `usercode` response was structurally invalid.
    ///
    /// REDACTION WARNING: `body` is the raw JSON response and may contain
    /// response data; redact before logging.
    #[error("Invalid OpenAI Codex device code response: {body}")]
    InvalidDeviceCodeResponse {
        /// The raw JSON that failed validation. May contain response data — redact before logging.
        body: String,
    },

    /// A terminal failure surfaced while polling the device-auth token endpoint.
    /// The message is preformatted by the polling closure.
    ///
    /// REDACTION WARNING: the wrapped message embeds the raw device-token error
    /// response body and may contain response data; redact before logging.
    #[error("{0}")]
    DeviceAuth(String),

    /// The device flow exceeded its deadline. Mirrors `Device flow timed out`.
    #[error("Device flow timed out")]
    DeviceTimeout,

    /// The device flow timed out after one or more `slow_down` responses.
    #[error(
        "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
    )]
    DeviceSlowDownTimeout,

    // -- Credential store --
    /// Could not determine the home directory for the default credential path.
    #[error("could not determine home directory (set $HOME or the GENAI_AUTH_FILE env override)")]
    HomeDirNotFound,

    /// No stored credential was found for the requested provider id.
    #[error("no stored credential for provider '{0}'")]
    NoCredential(String),
}
