//! Pinned Kimi Code RFC 8628 flow over M3.3 host contracts.

use agentprism_ai::{
    AuthChallengeId, AuthError, AuthEvent, AuthInteraction, AuthSource, CancellationToken,
    LocalBoxFuture, LocalHttpTransport, LocalOAuthAuth, LocalOAuthDeviceCodePoll,
    LocalOAuthDeviceCodePollOptions, OAuthAuth, OAuthCredential, OAuthDeviceCodePoll,
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, ProviderOAuthExtra, ResolvedAuth,
    SecretString, SendBoxFuture, Timestamp, poll_local_oauth_device_code_flow,
    poll_oauth_device_code_flow,
};
use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_HOST: &str = "https://auth.kimi.com";

/// Send-capable Kimi Code device OAuth.
pub struct KimiCodingOAuth {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    host: Url,
}

impl KimiCodingOAuth {
    /// Creates the pinned flow at Kimi's production OAuth host.
    pub fn new(transport: Arc<dyn agentprism_ai::HttpTransport>) -> Self {
        Self::with_host(transport, Url::parse(DEFAULT_HOST).expect("pinned URL"))
    }

    /// Creates the flow at an explicit host, matching `KIMI_CODE_OAUTH_HOST`.
    pub fn with_host(transport: Arc<dyn agentprism_ai::HttpTransport>, host: Url) -> Self {
        Self { transport, host }
    }

    /// Creates the flow using pinned Pi's construction-time host precedence:
    /// `KIMI_CODE_OAUTH_HOST`, legacy `KIMI_OAUTH_HOST`, then production.
    pub fn from_environment(
        transport: Arc<dyn agentprism_ai::HttpTransport>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self, AuthError> {
        Ok(Self::with_host(transport, oauth_host(environment)?))
    }
}

impl OAuthAuth for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let device =
                request_device_send(self.transport.as_ref(), &self.host, cancellation.clone())
                    .await?;
            notify_device(interaction.as_ref(), &device)?;
            let poll = KimiPoll {
                transport: self.transport.clone(),
                token_url: endpoint(&self.host, "api/oauth/token")?,
                device_code: device.device_code,
            };
            let mut options = OAuthDeviceCodePollOptions::new(Box::new(poll), cancellation);
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
            let url = endpoint(&self.host, "api/oauth/token")?;
            for attempt in 0..=3 {
                if attempt > 0 {
                    agentprism_ai::OAuthDeviceCodeRuntime::sleep(
                        &agentprism_ai::SystemOAuthDeviceCodeRuntime,
                        Duration::from_secs(1_u64 << (attempt - 1)),
                        cancellation.clone(),
                    )
                    .await?;
                }
                let request = agentprism_provider_common::form_post(
                    url.as_str(),
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", credential.refresh.expose_secret()),
                        ("client_id", CLIENT_ID),
                    ],
                )?;
                let (status, _, body) = match agentprism_provider_common::execute_send(
                    self.transport.as_ref(),
                    request,
                    cancellation.clone(),
                )
                .await
                {
                    Ok(response) => response,
                    Err(_) if attempt < 3 && !cancellation.is_cancelled() => continue,
                    Err(error) => return Err(error),
                };
                if (status == 429 || status >= 500) && attempt < 3 {
                    continue;
                }
                return parse_token(status, &body, None);
            }
            unreachable!("bounded refresh loop always returns")
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = bearer(credential);
        Box::pin(async move { result })
    }
}

/// Local-executor Kimi Code device OAuth.
pub struct LocalKimiCodingOAuth {
    transport: Rc<dyn LocalHttpTransport>,
    host: Url,
}

impl LocalKimiCodingOAuth {
    /// Creates the local flow at Kimi's production OAuth host.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self::with_host(transport, Url::parse(DEFAULT_HOST).expect("pinned URL"))
    }

    /// Creates the local flow at an explicit host.
    pub fn with_host(transport: Rc<dyn LocalHttpTransport>, host: Url) -> Self {
        Self { transport, host }
    }

    /// Local counterpart to [`KimiCodingOAuth::from_environment`].
    pub fn from_environment(
        transport: Rc<dyn LocalHttpTransport>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self, AuthError> {
        Ok(Self::with_host(transport, oauth_host(environment)?))
    }
}

impl LocalOAuthAuth for LocalKimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn login(
        &self,
        interaction: Rc<dyn agentprism_ai::LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let device =
                request_device_local(self.transport.as_ref(), &self.host, cancellation.clone())
                    .await?;
            notify_device(interaction.as_ref(), &device)?;
            let poll = LocalKimiPoll {
                transport: self.transport.clone(),
                token_url: endpoint(&self.host, "api/oauth/token")?,
                device_code: device.device_code,
            };
            let mut options = LocalOAuthDeviceCodePollOptions::new(Box::new(poll), cancellation);
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
            let url = endpoint(&self.host, "api/oauth/token")?;
            for attempt in 0..=3 {
                if attempt > 0 {
                    agentprism_ai::LocalOAuthDeviceCodeRuntime::sleep(
                        &agentprism_ai::SystemOAuthDeviceCodeRuntime,
                        Duration::from_secs(1_u64 << (attempt - 1)),
                        cancellation.clone(),
                    )
                    .await?;
                }
                let request = agentprism_provider_common::form_post(
                    url.as_str(),
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", credential.refresh.expose_secret()),
                        ("client_id", CLIENT_ID),
                    ],
                )?;
                let (status, _, body) = match agentprism_provider_common::execute_local(
                    self.transport.as_ref(),
                    request,
                    cancellation.clone(),
                )
                .await
                {
                    Ok(response) => response,
                    Err(_) if attempt < 3 && !cancellation.is_cancelled() => continue,
                    Err(error) => return Err(error),
                };
                if (status == 429 || status >= 500) && attempt < 3 {
                    continue;
                }
                return parse_token(status, &body, None);
            }
            unreachable!("bounded refresh loop always returns")
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = bearer(credential);
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

struct KimiPoll {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    token_url: Url,
    device_code: String,
}

impl OAuthDeviceCodePoll<OAuthCredential> for KimiPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = device_token_request(&self.token_url, &self.device_code)?;
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

struct LocalKimiPoll {
    transport: Rc<dyn LocalHttpTransport>,
    token_url: Url,
    device_code: String,
}

impl LocalOAuthDeviceCodePoll<OAuthCredential> for LocalKimiPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = device_token_request(&self.token_url, &self.device_code)?;
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

async fn request_device_send(
    transport: &dyn agentprism_ai::HttpTransport,
    host: &Url,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = agentprism_provider_common::form_post(
        endpoint(host, "api/oauth/device_authorization")?.as_str(),
        &[("client_id", CLIENT_ID)],
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_send(transport, request, cancellation).await?;
    parse_device(status, &body)
}

async fn request_device_local(
    transport: &dyn LocalHttpTransport,
    host: &Url,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = agentprism_provider_common::form_post(
        endpoint(host, "api/oauth/device_authorization")?.as_str(),
        &[("client_id", CLIENT_ID)],
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_local(transport, request, cancellation).await?;
    parse_device(status, &body)
}

fn device_token_request(
    url: &Url,
    device_code: &str,
) -> Result<agentprism_ai::HttpRequest, AuthError> {
    agentprism_provider_common::form_post(
        url.as_str(),
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
        ],
    )
}

fn parse_device(status: u16, body: &[u8]) -> Result<Device, AuthError> {
    let value = json(body)?;
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "kimi_oauth",
            format!("Kimi device authorization failed (HTTP {status})"),
        ));
    }
    trusted_http_url(&value, "verification_uri")?;
    let verification_uri_complete = trusted_http_url(&value, "verification_uri_complete")?;
    Ok(Device {
        device_code: field(&value, "device_code")?.into(),
        user_code: field(&value, "user_code")?.into(),
        verification_uri: verification_uri_complete,
        interval: Duration::from_secs(
            value
                .get("interval")
                .and_then(Value::as_u64)
                .filter(|interval| *interval > 0)
                .unwrap_or(5),
        ),
        expires_in: Duration::from_secs(
            value
                .get("expires_in")
                .and_then(Value::as_u64)
                .filter(|expires| *expires > 0)
                .unwrap_or(900),
        ),
    })
}

fn trusted_http_url(value: &Value, name: &str) -> Result<Url, AuthError> {
    let raw = field(value, name)?;
    let url =
        Url::parse(raw).map_err(|_| AuthError::new("kimi_oauth", format!("invalid {name}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AuthError::new("kimi_oauth", format!("untrusted {name}")));
    }
    Ok(url)
}

fn parse_poll(
    status: u16,
    body: &[u8],
) -> Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError> {
    if status >= 500 {
        let text = String::from_utf8_lossy(body);
        return Ok(OAuthDeviceCodePollResult::Failed {
            message: format!(
                "Kimi Code device token request failed with status {status}{}",
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                }
            ),
        });
    }

    let value = json_or_null(body);
    if (200..300).contains(&status) && value.get("access_token").and_then(Value::as_str).is_some() {
        return Ok(match parse_poll_token(&value) {
            Ok(credential) => OAuthDeviceCodePollResult::Complete(credential),
            Err(message) => OAuthDeviceCodePollResult::Failed { message },
        });
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
            "expired_token" => OAuthDeviceCodePollResult::Failed {
                message: "Kimi Code device authorization expired. Please restart login.".into(),
            },
            "access_denied" => OAuthDeviceCodePollResult::Failed {
                message: "Kimi Code login was denied.".into(),
            },
            error => {
                let description = value
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default();
                OAuthDeviceCodePollResult::Failed {
                    message: format!(
                        "Kimi Code device token request failed (status {status}){}",
                        if error.is_empty() {
                            String::new()
                        } else {
                            format!(": {error}{description}")
                        }
                    ),
                }
            }
        },
    )
}

fn parse_poll_token(value: &Value) -> Result<OAuthCredential, String> {
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires = value
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0);
    let (Some(access), Some(refresh), Some(expires)) = (access, refresh, expires) else {
        let json = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
        return Err(format!(
            "Kimi Code token poll response missing fields: {json}"
        ));
    };
    let lifetime_millis = if expires >= i64::MAX as f64 / 1000.0 {
        i64::MAX
    } else {
        (expires * 1000.0) as i64
    };
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(now_millis().saturating_add(lifetime_millis)),
        extra: ProviderOAuthExtra::None,
    })
}

fn parse_token(
    status: u16,
    body: &[u8],
    old_refresh: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
    let value = json(body)?;
    if !(200..300).contains(&status) {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(AuthError::new(
            "kimi_oauth",
            if matches!(status, 401 | 403) || error == "invalid_grant" {
                "Kimi OAuth unauthorized".into()
            } else {
                format!("Kimi OAuth token request failed (HTTP {status}): {error}")
            },
        ));
    }
    let access = field(&value, "access_token")?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .or(old_refresh)
        .ok_or_else(|| AuthError::new("kimi_oauth", "missing refresh_token"))?;
    let expires = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| AuthError::new("kimi_oauth", "missing or invalid expires_in"))?;
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(
            now_millis()
                .saturating_add(i64::try_from(expires.saturating_mul(1000)).unwrap_or(i64::MAX)),
        ),
        extra: ProviderOAuthExtra::None,
    })
}

fn notify_device<T: DeviceNotifier + ?Sized>(
    interaction: &T,
    device: &Device,
) -> Result<(), AuthError> {
    interaction.notify_device(AuthEvent::DeviceCode {
        challenge_id: AuthChallengeId::new("kimi-coding-device"),
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval: Some(device.interval),
        expires_in: Some(device.expires_in),
    })
}

trait DeviceNotifier {
    fn notify_device(&self, event: AuthEvent) -> Result<(), AuthError>;
}

impl DeviceNotifier for dyn AuthInteraction {
    fn notify_device(&self, event: AuthEvent) -> Result<(), AuthError> {
        self.notify(event).map_err(AuthError::from)
    }
}

impl DeviceNotifier for dyn agentprism_ai::LocalAuthInteraction {
    fn notify_device(&self, event: AuthEvent) -> Result<(), AuthError> {
        self.notify(event).map_err(AuthError::from)
    }
}

fn bearer(credential: &OAuthCredential) -> Result<ResolvedAuth, AuthError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", credential.access.expose_secret()))
            .map_err(|_| AuthError::new("kimi_oauth", "invalid access token"))?,
    );
    Ok(ResolvedAuth {
        api_key: None,
        headers,
        transport_headers: HeaderMap::new(),
        base_url: None,
        environment: Default::default(),
        source: AuthSource::new("OAuth"),
    })
}

fn endpoint(host: &Url, path: &str) -> Result<Url, AuthError> {
    Url::parse(&format!("{}/{}", host.as_str().trim_end_matches('/'), path))
        .map_err(|_| AuthError::new("kimi_oauth", "invalid OAuth host"))
}

fn oauth_host(environment: &BTreeMap<String, String>) -> Result<Url, AuthError> {
    let configured = environment
        .get("KIMI_CODE_OAUTH_HOST")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            environment
                .get("KIMI_OAUTH_HOST")
                .filter(|value| !value.is_empty())
        })
        .map(String::as_str)
        .unwrap_or(DEFAULT_HOST)
        .trim_end_matches('/');
    Url::parse(configured).map_err(|_| AuthError::new("kimi_oauth", "invalid OAuth host"))
}

fn json(body: &[u8]) -> Result<Value, AuthError> {
    serde_json::from_slice(body)
        .map_err(|_| AuthError::new("kimi_oauth", "invalid Kimi OAuth JSON"))
}

fn json_or_null(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, AuthError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::new("kimi_oauth", format!("missing {name}")))
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
