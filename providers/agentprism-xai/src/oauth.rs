//! Pinned xAI device OAuth over the portable M3.3 contracts.

use agentprism_ai::{
    AuthChallengeId, AuthError, AuthEvent, AuthInteraction, AuthSource, CancellationToken,
    LocalBoxFuture, LocalHttpTransport, LocalOAuthAuth, LocalOAuthDeviceCodePoll,
    LocalOAuthDeviceCodePollOptions, OAuthAuth, OAuthCredential, OAuthDeviceCodePoll,
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, ProviderOAuthExtra, ResolvedAuth,
    SecretString, SendBoxFuture, Timestamp, poll_local_oauth_device_code_flow,
    poll_oauth_device_code_flow,
};
use http::HeaderMap;
use serde_json::Value;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";

/// Send-capable xAI OAuth flow.
pub struct XaiOAuth {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
}

impl XaiOAuth {
    /// Creates the flow around an injected transport.
    pub fn new(transport: Arc<dyn agentprism_ai::HttpTransport>) -> Self {
        Self { transport }
    }
}

impl OAuthAuth for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let device = device_send(self.transport.as_ref(), cancellation.clone()).await?;
            notify_send(interaction.as_ref(), &device)?;
            let mut options = OAuthDeviceCodePollOptions::new(
                Box::new(XaiPoll {
                    transport: self.transport.clone(),
                    device_code: device.device_code,
                }),
                cancellation,
            );
            options.interval = Some(device.interval);
            options.expires_in = Some(device.expires_in);
            options.wait_before_first_poll = true;
            poll_oauth_device_code_flow(options).await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let request = refresh_request(credential.refresh.expose_secret())?;
            let (status, _, body) = agentprism_provider_common::execute_send(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_token(status, &body, Some(credential.refresh.expose_secret()))
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = resolved(credential);
        Box::pin(async move { result })
    }
}

/// Local-executor xAI OAuth flow.
pub struct LocalXaiOAuth {
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalXaiOAuth {
    /// Creates the local flow around an injected transport.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self { transport }
    }
}

impl LocalOAuthAuth for LocalXaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn login(
        &self,
        interaction: Rc<dyn agentprism_ai::LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let device = device_local(self.transport.as_ref(), cancellation.clone()).await?;
            notify_local(interaction.as_ref(), &device)?;
            let mut options = LocalOAuthDeviceCodePollOptions::new(
                Box::new(LocalXaiPoll {
                    transport: self.transport.clone(),
                    device_code: device.device_code,
                }),
                cancellation,
            );
            options.interval = Some(device.interval);
            options.expires_in = Some(device.expires_in);
            options.wait_before_first_poll = true;
            poll_local_oauth_device_code_flow(options).await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let request = refresh_request(credential.refresh.expose_secret())?;
            let (status, _, body) = agentprism_provider_common::execute_local(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_token(status, &body, Some(credential.refresh.expose_secret()))
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = resolved(credential);
        Box::pin(async move { result })
    }
}

struct Device {
    device_code: String,
    user_code: String,
    verification_uri: Url,
    interval: Duration,
    expires_in: Duration,
}

struct XaiPoll {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    device_code: String,
}

impl OAuthDeviceCodePoll<OAuthCredential> for XaiPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = device_token_request(&self.device_code)?;
            let (status, _, body) = agentprism_provider_common::execute_send(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_poll(status, &body)
        })
    }
}

struct LocalXaiPoll {
    transport: Rc<dyn LocalHttpTransport>,
    device_code: String,
}

impl LocalOAuthDeviceCodePoll<OAuthCredential> for LocalXaiPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = device_token_request(&self.device_code)?;
            let (status, _, body) = agentprism_provider_common::execute_local(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_poll(status, &body)
        })
    }
}

async fn device_send(
    transport: &dyn agentprism_ai::HttpTransport,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = device_request()?;
    let (status, _, body) =
        agentprism_provider_common::execute_send(transport, request, cancellation).await?;
    parse_device(status, &body)
}

async fn device_local(
    transport: &dyn LocalHttpTransport,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = device_request()?;
    let (status, _, body) =
        agentprism_provider_common::execute_local(transport, request, cancellation).await?;
    parse_device(status, &body)
}

fn device_request() -> Result<agentprism_ai::HttpRequest, AuthError> {
    agentprism_provider_common::form_post(
        DEVICE_URL,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "pi"),
        ],
    )
}

fn device_token_request(device_code: &str) -> Result<agentprism_ai::HttpRequest, AuthError> {
    agentprism_provider_common::form_post(
        TOKEN_URL,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
        ],
    )
}

fn refresh_request(refresh: &str) -> Result<agentprism_ai::HttpRequest, AuthError> {
    agentprism_provider_common::form_post(
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh),
        ],
    )
}

fn parse_device(status: u16, body: &[u8]) -> Result<Device, AuthError> {
    let value = json(body)?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "xai_oauth",
            request_failure("device authorization", status, &value),
        ));
    }
    let base_verification_uri = trusted_verification_uri(field(&value, "verification_uri")?)?;
    let complete_verification_uri = value
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(trusted_verification_uri)
        .transpose()?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AuthError::new("xai_oauth", "Invalid xAI OAuth response field: expires_in")
        })?;
    Ok(Device {
        device_code: field(&value, "device_code")?.into(),
        user_code: field(&value, "user_code")?.into(),
        verification_uri: complete_verification_uri.unwrap_or(base_verification_uri),
        interval: Duration::from_secs(
            value
                .get("interval")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .unwrap_or(5),
        ),
        expires_in: Duration::from_secs(expires_in),
    })
}

fn parse_poll(
    status: u16,
    body: &[u8],
) -> Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError> {
    let value = json(body)?;
    if (200..300).contains(&status) {
        return parse_token(status, body, None).map(OAuthDeviceCodePollResult::Complete);
    }
    Ok(
        match value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "authorization_pending" => OAuthDeviceCodePollResult::Pending,
            "slow_down" => OAuthDeviceCodePollResult::SlowDown {
                interval: value
                    .get("interval")
                    .and_then(Value::as_u64)
                    .map(Duration::from_secs),
            },
            "access_denied" | "authorization_denied" => OAuthDeviceCodePollResult::Failed {
                message: "xAI device authorization was denied".into(),
            },
            "expired_token" => OAuthDeviceCodePollResult::Failed {
                message: "xAI device code expired".into(),
            },
            _ => OAuthDeviceCodePollResult::Failed {
                message: request_failure("device token polling", status, &value),
            },
        },
    )
}

fn parse_token(
    status: u16,
    body: &[u8],
    old_refresh: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
    let value = json(body)?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "xai_oauth",
            request_failure("token refresh", status, &value),
        ));
    }
    let access = field(&value, "access_token")?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .or(old_refresh)
        .ok_or_else(|| {
            AuthError::new(
                "xai_oauth",
                "Invalid xAI OAuth response field: refresh_token",
            )
        })?;
    let expires = match value.get("expires_in") {
        None => 3600,
        Some(value) => value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
            AuthError::new("xai_oauth", "Invalid xAI OAuth response field: expires_in")
        })?,
    };
    let lifetime = i64::try_from(expires.saturating_mul(1000)).unwrap_or(i64::MAX);
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(
            now_millis()
                .saturating_add(lifetime)
                .saturating_sub(300_000),
        ),
        extra: ProviderOAuthExtra::None,
    })
}

fn notify_send(interaction: &dyn AuthInteraction, device: &Device) -> Result<(), AuthError> {
    interaction.notify(event(device)).map_err(AuthError::from)
}

fn trusted_verification_uri(raw: &str) -> Result<Url, AuthError> {
    let url =
        Url::parse(raw).map_err(|_| AuthError::new("xai_oauth", "Untrusted verification URI"))?;
    if url.scheme() != "https" {
        return Err(AuthError::new("xai_oauth", "Untrusted verification URI"));
    }
    Ok(url)
}

fn notify_local(
    interaction: &dyn agentprism_ai::LocalAuthInteraction,
    device: &Device,
) -> Result<(), AuthError> {
    interaction.notify(event(device)).map_err(AuthError::from)
}

fn event(device: &Device) -> AuthEvent {
    AuthEvent::DeviceCode {
        challenge_id: AuthChallengeId::new("xai-device"),
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval: Some(device.interval),
        expires_in: Some(device.expires_in),
    }
}

fn resolved(credential: &OAuthCredential) -> Result<ResolvedAuth, AuthError> {
    Ok(ResolvedAuth {
        api_key: Some(credential.access.clone()),
        headers: HeaderMap::new(),
        transport_headers: HeaderMap::new(),
        base_url: None,
        source: AuthSource::new("OAuth"),
    })
}

fn json(body: &[u8]) -> Result<Value, AuthError> {
    serde_json::from_slice(body).map_err(|_| AuthError::new("xai_oauth", "invalid xAI OAuth JSON"))
}

fn request_failure(action: &str, status: u16, body: &Value) -> String {
    let error = body.get("error").and_then(Value::as_str);
    let description = body.get("error_description").and_then(Value::as_str);
    let detail = match (error, description) {
        (Some(error), Some(description)) => format!(": {error}: {description}"),
        (Some(error), None) => format!(": {error}"),
        (None, Some(description)) => format!(": {description}"),
        (None, None) => String::new(),
    };
    format!("xAI OAuth {action} failed (HTTP {status}){detail}")
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, AuthError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AuthError::new(
                "xai_oauth",
                format!("Invalid xAI OAuth response field: {name}"),
            )
        })
}

fn now_millis() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
