pub mod anthropic;
pub mod device_code;
pub mod github_copilot;
pub mod kimi_coding;
pub mod load;
pub mod oauth_page;
pub mod openai_codex;
pub mod openrouter;
pub mod pkce;
pub mod radius;
pub mod xai;

use crate::types::{AbortSignal, FetchFunction, ProviderHttpRequest};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub(crate) enum OAuthHttpError {
    Aborted,
    Timeout,
    Transport(String),
}

impl fmt::Display for OAuthHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aborted => formatter.write_str("request aborted"),
            Self::Timeout => formatter.write_str("request timed out"),
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

pub(crate) struct OAuthHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl OAuthHttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub(crate) async fn send_http(
    fetch: Arc<dyn FetchFunction>,
    request: ProviderHttpRequest,
    timeout: Option<Duration>,
) -> Result<OAuthHttpResponse, OAuthHttpError> {
    let signal = request.signal.clone();
    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
        return Err(OAuthHttpError::Aborted);
    }
    let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
    let send = fetch.fetch(request);
    tokio::pin!(send);
    let response = match (signal.as_ref(), deadline) {
        (Some(signal), Some(deadline)) => tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(OAuthHttpError::Aborted),
            _ = tokio::time::sleep_until(deadline) => return Err(OAuthHttpError::Timeout),
            response = &mut send => response,
        },
        (Some(signal), None) => tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(OAuthHttpError::Aborted),
            response = &mut send => response,
        },
        (None, Some(deadline)) => tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err(OAuthHttpError::Timeout),
            response = &mut send => response,
        },
        (None, None) => send.await,
    }
    .map_err(|error| {
        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
            OAuthHttpError::Aborted
        } else {
            OAuthHttpError::Transport(error)
        }
    })?;

    let mut bytes = Vec::new();
    if let Some(mut body) = response.body {
        loop {
            let next = match (signal.as_ref(), deadline) {
                (Some(signal), Some(deadline)) => tokio::select! {
                    biased;
                    _ = signal.cancelled() => return Err(OAuthHttpError::Aborted),
                    _ = tokio::time::sleep_until(deadline) => return Err(OAuthHttpError::Timeout),
                    chunk = body.next() => chunk,
                },
                (Some(signal), None) => tokio::select! {
                    biased;
                    _ = signal.cancelled() => return Err(OAuthHttpError::Aborted),
                    chunk = body.next() => chunk,
                },
                (None, Some(deadline)) => tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return Err(OAuthHttpError::Timeout),
                    chunk = body.next() => chunk,
                },
                (None, None) => body.next().await,
            };
            let Some(chunk) = next else {
                break;
            };
            bytes.extend(chunk.map_err(OAuthHttpError::Transport)?);
        }
    }

    Ok(OAuthHttpResponse {
        status: response.status,
        status_text: response.status_text,
        headers: response.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

pub(crate) fn request(
    method: &str,
    url: String,
    headers: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    body: impl Into<Vec<u8>>,
    signal: Arc<dyn AbortSignal>,
) -> ProviderHttpRequest {
    ProviderHttpRequest {
        method: method.to_owned(),
        url,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
        body: Some(body.into()),
        signal: Some(signal),
    }
}

pub(crate) fn form(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(fields.iter().copied());
    serializer.finish()
}

pub(crate) fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64()
        * 1_000.0
}

pub(crate) fn random_uuid_v4() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

pub(crate) struct LoopbackRequest {
    pub method: String,
    pub target: String,
}

pub(crate) async fn read_loopback_request(
    stream: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<LoopbackRequest> {
    let mut bytes = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") || bytes.len() >= 65_536 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&bytes);
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    Ok(LoopbackRequest {
        method: parts.next().unwrap_or_default().to_owned(),
        target: parts.next().unwrap_or("/").to_owned(),
    })
}

pub(crate) async fn write_loopback_response(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    content_type: &str,
    cache_control: Option<&str>,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "",
    };
    let cache_header = cache_control
        .map(|value| format!("Cache-Control: {value}\r\n"))
        .unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n{cache_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::types::{
        FetchFunction, ProviderBodyStream, ProviderHttpRequest, ProviderHttpResponse,
    };
    use futures::future::BoxFuture;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    pub(crate) type FetchHandler =
        dyn Fn(ProviderHttpRequest) -> Result<ProviderHttpResponse, String> + Send + Sync;

    pub(crate) struct TestFetch(pub Arc<FetchHandler>);

    impl FetchFunction for TestFetch {
        fn fetch(
            &self,
            request: ProviderHttpRequest,
        ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            let result = (self.0)(request);
            Box::pin(async move { result })
        }
    }

    pub(crate) fn fetch(
        handler: impl Fn(ProviderHttpRequest) -> Result<ProviderHttpResponse, String>
        + Send
        + Sync
        + 'static,
    ) -> Arc<dyn FetchFunction> {
        Arc::new(TestFetch(Arc::new(handler)))
    }

    pub(crate) fn response(status: u16, body: impl Into<String>) -> ProviderHttpResponse {
        response_with_headers(status, body, BTreeMap::new())
    }

    pub(crate) fn response_with_headers(
        status: u16,
        body: impl Into<String>,
        headers: BTreeMap<String, String>,
    ) -> ProviderHttpResponse {
        let status_text = http::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or_default()
            .to_owned();
        let bytes = body.into().into_bytes();
        let body: ProviderBodyStream = Box::pin(futures::stream::once(async move { Ok(bytes) }));
        ProviderHttpResponse {
            status,
            status_text,
            headers,
            body: Some(body),
        }
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::anthropic::anthropic_oauth;
    use super::github_copilot::github_copilot_oauth;
    use super::kimi_coding::kimi_coding_oauth;
    use super::openai_codex::openai_codex_oauth;
    use super::openrouter::openrouter_oauth;
    use super::xai::xai_oauth;
    use super::{read_loopback_request, write_loopback_response};
    use crate::auth::{OAuthCredential, OAuthCredentialType};
    use crate::utils::abort::AbortController;
    use serde_json::Map;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn credential(access: &str) -> OAuthCredential {
        OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: access.to_owned(),
            expires: 0.0,
            extra: Map::new(),
        }
    }

    /// Ports pi `test/oauth-auth.test.ts:30`, `:37`, `:42`, `:47`, and `:53`.
    #[tokio::test]
    async fn subscription_flags_and_api_key_adapters_match() {
        for oauth in [
            anthropic_oauth(),
            openai_codex_oauth(),
            github_copilot_oauth(),
            kimi_coding_oauth(),
            xai_oauth(),
        ] {
            assert_eq!(oauth.is_subscription, Some(true));
        }
        let openrouter = openrouter_oauth();
        assert_eq!(openrouter.is_subscription, None);
        for oauth in [anthropic_oauth(), openai_codex_oauth(), xai_oauth()] {
            assert_eq!(
                (oauth.to_auth)(credential("token"))
                    .await
                    .expect("auth")
                    .api_key
                    .as_deref(),
                Some("token")
            );
        }
        let permanent = credential("openrouter-key");
        assert_eq!(
            (openrouter.refresh)(permanent.clone(), AbortController::new().signal())
                .await
                .expect("refresh"),
            permanent
        );
    }

    /// Pins pi `src/auth/oauth/openrouter.ts:45-50` through the Rust loopback HTTP adapter.
    #[tokio::test]
    async fn loopback_http_adapter_reads_requests_and_writes_one_complete_response() {
        let (mut client, mut server) = tokio::io::duplex(4_096);
        client
            .write_all(b"GET /callback?code=a HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("request");
        let request = read_loopback_request(&mut server).await.expect("parsed");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/callback?code=a");

        write_loopback_response(
            &mut server,
            200,
            "text/plain; charset=utf-8",
            Some("no-store"),
            "ok",
        )
        .await
        .expect("response");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("response body");
        assert_eq!(response.matches("HTTP/1.1 200 OK").count(), 1);
        assert!(response.ends_with("\r\n\r\nok"));
    }
}
