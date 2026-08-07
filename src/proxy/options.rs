//! Validation and secret-bearing connection settings for the proxy transport.
//!
//! Base URLs may use HTTP or HTTPS but may not include userinfo, a query, or a fragment. Use HTTPS
//! outside loopback/development; see the
//! [crate README's proxy guidance](https://docs.rs/crate/rust-genai-agent/latest#testing-and-proxying).
//! The client constructed here disables redirects. Replacing it transfers redirect-policy and
//! endpoint-trust responsibility to the caller.

use reqwest::header::HeaderValue;
use reqwest::{Client, Url};

/// Invalid local configuration for the proxy transport.
///
/// Configuration is validated before a [`crate::StreamFn`] is constructed. Runtime request and
/// protocol failures remain in-band assistant errors instead.
#[derive(Debug, thiserror::Error)]
pub enum ProxyConfigError {
    /// The base URL could not be parsed or used as a hierarchical URL.
    #[error("invalid proxy base URL: {reason}")]
    InvalidUrl {
        /// Parser or normalization diagnostic; the bearer token is never included.
        reason: String,
    },

    /// The base URL used a scheme other than HTTP or HTTPS.
    #[error("proxy URL scheme must be http or https, got {scheme}")]
    UnsupportedScheme {
        /// Rejected URL scheme.
        scheme: String,
    },

    /// The base URL contained a query component.
    #[error("proxy base URL must not contain a query")]
    QueryNotAllowed,

    /// The base URL contained a fragment component.
    #[error("proxy base URL must not contain a fragment")]
    FragmentNotAllowed,

    /// The base URL contained username or password userinfo, including encoded userinfo.
    ///
    /// Credentials belong only in the separately validated bearer-token argument.
    #[error("proxy base URL must not contain userinfo")]
    UserInfoNotAllowed,

    /// Construction of the built-in, no-redirect HTTP client failed.
    #[error("failed to build the default proxy HTTP client: {reason}")]
    ClientBuildFailed {
        /// Diagnostic reported by the HTTP-client builder.
        reason: String,
    },

    /// The bearer token was empty or consisted only of whitespace.
    #[error("proxy auth token must not be empty")]
    EmptyAuthToken,

    /// The bearer token contained bytes invalid in an HTTP `Authorization` header value.
    #[error("proxy auth token is not valid in an HTTP Authorization header")]
    InvalidAuthToken,
}

/// Validated connection settings for one proxy endpoint.
///
/// The base URL is normalized once to an endpoint ending in `/api/stream`. URL userinfo, queries,
/// and fragments are rejected. Authentication is intentionally private and its `Debug`
/// representation is always redacted, but clones still contain the bearer-token secret.
///
/// Options built by [`Self::new`] use an HTTP client with redirects disabled. A client installed by
/// [`Self::with_client`] keeps its caller-selected redirect policy. Use HTTPS for production proxy
/// endpoints.
#[derive(Clone)]
pub struct ProxyStreamOptions {
    endpoint: Url,
    auth_token: String,
    client: Client,
}

impl ProxyStreamOptions {
    /// Validate a base URL and bearer token and construct proxy options.
    ///
    /// The path is normalized by appending `/api/stream`. The URL must be hierarchical, use `http`
    /// or `https`, and contain no userinfo, query, or fragment. The built-in client does not follow
    /// redirects. Plain HTTP is accepted for loopback/development, but production endpoints should
    /// use HTTPS.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyConfigError`] when URL parsing/normalization, token validation, or construction
    /// of the built-in client fails.
    pub fn new(
        base_url: impl AsRef<str>,
        auth_token: impl Into<String>,
    ) -> Result<Self, ProxyConfigError> {
        let mut endpoint =
            Url::parse(base_url.as_ref()).map_err(|error| ProxyConfigError::InvalidUrl {
                reason: error.to_string(),
            })?;
        match endpoint.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(ProxyConfigError::UnsupportedScheme {
                    scheme: scheme.to_owned(),
                });
            }
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(ProxyConfigError::UserInfoNotAllowed);
        }
        if endpoint.query().is_some() {
            return Err(ProxyConfigError::QueryNotAllowed);
        }
        if endpoint.fragment().is_some() {
            return Err(ProxyConfigError::FragmentNotAllowed);
        }
        {
            let mut segments =
                endpoint
                    .path_segments_mut()
                    .map_err(|_| ProxyConfigError::InvalidUrl {
                        reason: "URL cannot be used as a hierarchical base".to_owned(),
                    })?;
            segments.pop_if_empty();
            segments.push("api");
            segments.push("stream");
        }

        let auth_token = validate_auth_token(auth_token.into())?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ProxyConfigError::ClientBuildFailed {
                reason: error.to_string(),
            })?;
        Ok(Self {
            endpoint,
            auth_token,
            client,
        })
    }

    /// Replace the HTTP client used by this proxy transport.
    ///
    /// This also replaces the built-in client's no-redirect policy. The injected client retains
    /// its caller-selected redirect, TLS, timeout, proxy, and related policies; the caller must
    /// ensure those policies cannot disclose the bearer token to an unintended endpoint.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Rotate the bearer token while preserving the endpoint and HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyConfigError::EmptyAuthToken`] or [`ProxyConfigError::InvalidAuthToken`] when
    /// the replacement cannot be used as a bearer `Authorization` header value.
    pub fn with_auth_token(
        mut self,
        auth_token: impl Into<String>,
    ) -> Result<Self, ProxyConfigError> {
        self.auth_token = validate_auth_token(auth_token.into())?;
        Ok(self)
    }

    /// Return the normalized endpoint whose path ends in `/api/stream`.
    ///
    /// The returned URL has no userinfo, query, or fragment because those components are rejected
    /// during construction.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub(crate) fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }
}

impl std::fmt::Debug for ProxyStreamOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyStreamOptions")
            .field("endpoint", &self.endpoint)
            .field("auth_token", &"[REDACTED]")
            .field("client", &"reqwest::Client")
            .finish()
    }
}

fn validate_auth_token(auth_token: String) -> Result<String, ProxyConfigError> {
    if auth_token.trim().is_empty() {
        return Err(ProxyConfigError::EmptyAuthToken);
    }
    let value = format!("Bearer {auth_token}");
    HeaderValue::from_bytes(value.as_bytes()).map_err(|_| ProxyConfigError::InvalidAuthToken)?;
    Ok(auth_token)
}
