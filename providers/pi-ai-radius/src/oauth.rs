//! Radius browser and RFC 8628 OAuth over the portable M3.3 host contracts.

use http::{HeaderMap, HeaderValue, header};
use pi_ai::{
    AuthAnswer, AuthChallengeId, AuthError, AuthEvent, AuthHtmlPage, AuthInteraction, AuthPrompt,
    AuthSelectOption, AuthSource, CancellationToken, LocalAuthInteraction, LocalBoxFuture,
    LocalHttpTransport, LocalOAuthAuth, LocalOAuthDeviceCodePoll, LocalOAuthDeviceCodePollOptions,
    OAuthAuth, OAuthCredential, OAuthDeviceCodePoll, OAuthDeviceCodePollOptions,
    OAuthDeviceCodePollResult, ProviderOAuthExtra, RedirectReceiverRequest, RedirectStrategy,
    RedirectStrategyDescription, ResolvedAuth, SecretString, SendBoxFuture, Timestamp,
    generate_oauth_state, generate_pkce, poll_local_oauth_device_code_flow,
    poll_oauth_device_code_flow, validate_oauth_state,
};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Send-capable Radius OAuth flow bound to one gateway.
pub struct RadiusOAuth {
    transport: Arc<dyn pi_ai::HttpTransport>,
    gateway: Url,
}

impl RadiusOAuth {
    /// Creates a Radius OAuth flow for the selected gateway.
    pub fn new(transport: Arc<dyn pi_ai::HttpTransport>, gateway: Url) -> Self {
        Self { transport, gateway }
    }
}

impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        "Radius"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let method = select_send(interaction.as_ref(), cancellation.clone()).await?;
            match method.as_str() {
                "device-code" => {
                    device_login_send(
                        Arc::clone(&self.transport),
                        self.gateway.clone(),
                        interaction,
                        cancellation,
                    )
                    .await
                }
                "browser" => {
                    browser_login_send(
                        Arc::clone(&self.transport),
                        self.gateway.clone(),
                        interaction,
                        cancellation,
                    )
                    .await
                }
                other => Err(AuthError::new(
                    "radius_oauth",
                    format!("Unknown Radius sign-in method: {other}"),
                )),
            }
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let request = pi_ai_provider_common::form_post(
                endpoint(&self.gateway, "/v1/oauth/token")?.as_str(),
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", credential.refresh.expose_secret()),
                ],
            )?;
            let (status, _, body) =
                pi_ai_provider_common::execute_send(self.transport.as_ref(), request, cancellation)
                    .await?;
            parse_token(status, &body, &self.gateway)
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = radius_auth(credential);
        Box::pin(async move { result })
    }
}

/// Local-executor Radius OAuth flow.
pub struct LocalRadiusOAuth {
    transport: Rc<dyn LocalHttpTransport>,
    gateway: Url,
}

impl LocalRadiusOAuth {
    /// Creates a local Radius OAuth flow for the selected gateway.
    pub fn new(transport: Rc<dyn LocalHttpTransport>, gateway: Url) -> Self {
        Self { transport, gateway }
    }
}

impl LocalOAuthAuth for LocalRadiusOAuth {
    fn name(&self) -> &str {
        "Radius"
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let method = select_local(interaction.as_ref(), cancellation.clone()).await?;
            match method.as_str() {
                "device-code" => {
                    device_login_local(
                        Rc::clone(&self.transport),
                        self.gateway.clone(),
                        interaction,
                        cancellation,
                    )
                    .await
                }
                "browser" => {
                    browser_login_local(
                        Rc::clone(&self.transport),
                        self.gateway.clone(),
                        interaction,
                        cancellation,
                    )
                    .await
                }
                other => Err(AuthError::new(
                    "radius_oauth",
                    format!("Unknown Radius sign-in method: {other}"),
                )),
            }
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let request = pi_ai_provider_common::form_post(
                endpoint(&self.gateway, "/v1/oauth/token")?.as_str(),
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", credential.refresh.expose_secret()),
                ],
            )?;
            let (status, _, body) = pi_ai_provider_common::execute_local(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_token(status, &body, &self.gateway)
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = radius_auth(credential);
        Box::pin(async move { result })
    }
}

fn login_prompt() -> AuthPrompt {
    AuthPrompt::Select {
        message: "Sign in to Radius:".into(),
        options: vec![
            AuthSelectOption {
                id: "browser".into(),
                label: "Sign in with browser (recommended)".into(),
                description: None,
            },
            AuthSelectOption {
                id: "device-code".into(),
                label: "Sign in with device code (when signing in from another device)".into(),
                description: None,
            },
        ],
    }
}

async fn select_send(
    interaction: &dyn AuthInteraction,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    match interaction.prompt(login_prompt(), cancellation).await? {
        AuthAnswer::Selected(value) => Ok(value),
        _ => Err(AuthError::new(
            "radius_oauth",
            "Radius login selection returned a non-selection answer",
        )),
    }
}

async fn select_local(
    interaction: &dyn LocalAuthInteraction,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    match interaction.prompt(login_prompt(), cancellation).await? {
        AuthAnswer::Selected(value) => Ok(value),
        _ => Err(AuthError::new(
            "radius_oauth",
            "Radius login selection returned a non-selection answer",
        )),
    }
}

#[derive(Clone)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: Url,
    expires_in: Duration,
    interval: Option<Duration>,
}

async fn device_login_send(
    transport: Arc<dyn pi_ai::HttpTransport>,
    gateway: Url,
    interaction: Arc<dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let device = request_device_send(transport.as_ref(), &gateway, cancellation.clone()).await?;
    interaction.notify(device_event(&device))?;
    let poll = RadiusPoll {
        transport,
        gateway,
        device_code: device.device_code,
    };
    let mut options = OAuthDeviceCodePollOptions::new(Box::new(poll), cancellation);
    options.interval = device.interval;
    options.expires_in = Some(device.expires_in);
    poll_oauth_device_code_flow(options).await
}

async fn device_login_local(
    transport: Rc<dyn LocalHttpTransport>,
    gateway: Url,
    interaction: Rc<dyn LocalAuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let device = request_device_local(transport.as_ref(), &gateway, cancellation.clone()).await?;
    interaction.notify(device_event(&device))?;
    let poll = LocalRadiusPoll {
        transport,
        gateway,
        device_code: device.device_code,
    };
    let mut options = LocalOAuthDeviceCodePollOptions::new(Box::new(poll), cancellation);
    options.interval = device.interval;
    options.expires_in = Some(device.expires_in);
    poll_local_oauth_device_code_flow(options).await
}

struct RadiusPoll {
    transport: Arc<dyn pi_ai::HttpTransport>,
    gateway: Url,
    device_code: String,
}
impl OAuthDeviceCodePoll<OAuthCredential> for RadiusPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = token_device_request(&self.gateway, &self.device_code)?;
            let (status, _, body) =
                pi_ai_provider_common::execute_send(self.transport.as_ref(), request, cancellation)
                    .await?;
            parse_poll(status, &body, &self.gateway)
        })
    }
}

struct LocalRadiusPoll {
    transport: Rc<dyn LocalHttpTransport>,
    gateway: Url,
    device_code: String,
}
impl LocalOAuthDeviceCodePoll<OAuthCredential> for LocalRadiusPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            let request = token_device_request(&self.gateway, &self.device_code)?;
            let (status, _, body) = pi_ai_provider_common::execute_local(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_poll(status, &body, &self.gateway)
        })
    }
}

async fn request_device_send(
    transport: &dyn pi_ai::HttpTransport,
    gateway: &Url,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = device_request(gateway)?;
    let (status, _, body) =
        pi_ai_provider_common::execute_send(transport, request, cancellation).await?;
    parse_device(status, &body)
}

async fn request_device_local(
    transport: &dyn LocalHttpTransport,
    gateway: &Url,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = device_request(gateway)?;
    let (status, _, body) =
        pi_ai_provider_common::execute_local(transport, request, cancellation).await?;
    parse_device(status, &body)
}

fn device_request(gateway: &Url) -> Result<pi_ai::HttpRequest, AuthError> {
    pi_ai_provider_common::form_post(
        endpoint(gateway, "/v1/oauth/device")?.as_str(),
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
    )
}

fn token_device_request(gateway: &Url, code: &str) -> Result<pi_ai::HttpRequest, AuthError> {
    pi_ai_provider_common::form_post(
        endpoint(gateway, "/v1/oauth/token")?.as_str(),
        &[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLIENT_ID),
            ("device_code", code),
        ],
    )
}

fn parse_device(status: u16, body: &[u8]) -> Result<Device, AuthError> {
    let value = json(status, body, "Radius OAuth device authorization failed")?;
    let required = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AuthError::new(
                    "radius_oauth",
                    "Radius OAuth device authorization response is missing required fields",
                )
            })
    };
    let verification_uri = Url::parse(&required("verification_uri")?).map_err(|_| {
        AuthError::new(
            "radius_oauth",
            "Radius OAuth device authorization response is missing required fields",
        )
    })?;
    let expires = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .ok_or_else(|| {
            AuthError::new(
                "radius_oauth",
                "Radius OAuth device authorization response is missing required fields",
            )
        })?;
    Ok(Device {
        device_code: required("device_code")?,
        user_code: required("user_code")?,
        verification_uri,
        expires_in: Duration::from_secs(expires),
        interval: value
            .get("interval")
            .and_then(Value::as_u64)
            .map(Duration::from_secs),
    })
}

fn parse_poll(
    status: u16,
    body: &[u8],
    gateway: &Url,
) -> Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError> {
    if (200..300).contains(&status) {
        return parse_token(status, body, gateway).map(OAuthDeviceCodePollResult::Complete);
    }
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    Ok(
        match value.get("error").and_then(Value::as_str).unwrap_or("") {
            "authorization_pending" => OAuthDeviceCodePollResult::Pending,
            "slow_down" => OAuthDeviceCodePollResult::SlowDown { interval: None },
            "expired_token" => OAuthDeviceCodePollResult::Failed {
                message: "Device authorization expired.".into(),
            },
            "access_denied" => OAuthDeviceCodePollResult::Failed {
                message: "Device authorization was denied.".into(),
            },
            error => {
                return Err(oauth_response_error(
                    status,
                    &value,
                    "Radius OAuth token request failed",
                    error,
                ));
            }
        },
    )
}

fn parse_token(status: u16, body: &[u8], gateway: &Url) -> Result<OAuthCredential, AuthError> {
    let value = json(status, body, "Radius OAuth token request failed")?;
    let access = required_string(&value, "access_token")?;
    let refresh = required_string(&value, "refresh_token")?;
    let expires = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            AuthError::new(
                "radius_oauth",
                "Radius OAuth token response is missing expires_in",
            )
        })?;
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(
            now()
                .saturating_add(i64::try_from(expires.saturating_mul(1000)).unwrap_or(i64::MAX))
                .saturating_sub(60_000),
        ),
        extra: ProviderOAuthExtra::Radius {
            gateway_url: gateway.clone(),
            organization_id: None,
        },
    })
}

async fn browser_login_send(
    transport: Arc<dyn pi_ai::HttpTransport>,
    gateway: Url,
    interaction: Arc<dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let authorization = discovery_send(transport.as_ref(), &gateway, cancellation.clone()).await?;
    if !interaction.capabilities().loopback_http {
        return Err(unsupported_redirect(interaction.capabilities()));
    }
    let pkce = generate_pkce()?;
    let state = generate_oauth_state()?;
    let challenge_id = AuthChallengeId::new(state.clone());
    let receiver = interaction
        .create_redirect_receiver(redirect_request(challenge_id.clone()), cancellation.clone())
        .await
        .map_err(AuthError::from)?;
    let redirect_uri = receiver.redirect_uri().clone();
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OAuth callback on {redirect_uri}"),
    })?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorize_url(authorization, &redirect_uri, &state, &pkce.challenge)?,
        instructions: Some("Continue in your browser.".into()),
    })?;
    let arrival = receiver.receive(cancellation.clone()).await?;
    let code = callback_code(&arrival.url, &state)?;
    exchange_browser_send(
        transport.as_ref(),
        &gateway,
        &redirect_uri,
        &code,
        &pkce.verifier,
        cancellation,
    )
    .await
}

async fn browser_login_local(
    transport: Rc<dyn LocalHttpTransport>,
    gateway: Url,
    interaction: Rc<dyn LocalAuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let authorization = discovery_local(transport.as_ref(), &gateway, cancellation.clone()).await?;
    if !interaction.capabilities().loopback_http {
        return Err(unsupported_redirect(interaction.capabilities()));
    }
    let pkce = generate_pkce()?;
    let state = generate_oauth_state()?;
    let challenge_id = AuthChallengeId::new(state.clone());
    let receiver = interaction
        .create_redirect_receiver(redirect_request(challenge_id.clone()), cancellation.clone())
        .await
        .map_err(AuthError::from)?;
    let redirect_uri = receiver.redirect_uri().clone();
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OAuth callback on {redirect_uri}"),
    })?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorize_url(authorization, &redirect_uri, &state, &pkce.challenge)?,
        instructions: Some("Continue in your browser.".into()),
    })?;
    let arrival = receiver.receive(cancellation.clone()).await?;
    let code = callback_code(&arrival.url, &state)?;
    exchange_browser_local(
        transport.as_ref(),
        &gateway,
        &redirect_uri,
        &code,
        &pkce.verifier,
        cancellation,
    )
    .await
}

fn redirect_request(challenge_id: AuthChallengeId) -> RedirectReceiverRequest {
    RedirectReceiverRequest {
        challenge_id,
        preferred: vec![RedirectStrategy::FixedLoopback {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 1456,
            path: "/oauth/callback".into(),
        }],
        expected_path: Some("/oauth/callback".into()),
        success_page: AuthHtmlPage {
            html: "Signed in to Radius. You may now close this page.".into(),
        },
        failure_page: AuthHtmlPage {
            html: "Radius OAuth authentication failed.".into(),
        },
    }
}

fn unsupported_redirect(capabilities: pi_ai::AuthHostCapabilities) -> AuthError {
    AuthError::UnsupportedRedirectStrategy {
        provider: pi_ai::ProviderId::new("radius"),
        required: RedirectStrategyDescription {
            required: vec![RedirectStrategy::FixedLoopback {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 1456,
                path: "/oauth/callback".into(),
            }],
        },
        host_capabilities: capabilities,
    }
}

async fn discovery_send(
    transport: &dyn pi_ai::HttpTransport,
    gateway: &Url,
    cancellation: CancellationToken,
) -> Result<Url, AuthError> {
    let request = get_json(endpoint(gateway, "/v1/oauth")?);
    let (status, _, body) =
        pi_ai_provider_common::execute_send(transport, request, cancellation).await?;
    parse_discovery(status, &body, gateway)
}

async fn discovery_local(
    transport: &dyn LocalHttpTransport,
    gateway: &Url,
    cancellation: CancellationToken,
) -> Result<Url, AuthError> {
    let request = get_json(endpoint(gateway, "/v1/oauth")?);
    let (status, _, body) =
        pi_ai_provider_common::execute_local(transport, request, cancellation).await?;
    parse_discovery(status, &body, gateway)
}

fn parse_discovery(status: u16, body: &[u8], gateway: &Url) -> Result<Url, AuthError> {
    let value = json(
        status,
        body,
        &format!("Could not load Radius OAuth config from {gateway}"),
    )?;
    Url::parse(
        value
            .get("authorizationEndpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AuthError::new(
                    "radius_oauth",
                    format!("Invalid Radius OAuth config from {gateway}"),
                )
            })?,
    )
    .map_err(|_| {
        AuthError::new(
            "radius_oauth",
            format!("Invalid Radius OAuth config from {gateway}"),
        )
    })
}

fn authorize_url(
    mut url: Url,
    redirect: &Url,
    state: &str,
    challenge: &str,
) -> Result<Url, AuthError> {
    url.query_pairs_mut()
        .clear()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect.as_str())
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("handoff", "url")
        .append_pair("state", state);
    Ok(url)
}

fn callback_code(url: &Url, expected_state: &str) -> Result<String, AuthError> {
    let state = url
        .query_pairs()
        .find_map(|(k, v)| (k == "state").then(|| v.into_owned()))
        .ok_or(AuthError::StateMismatch)?;
    validate_oauth_state(expected_state, &state)?;
    if let Some(error) = url
        .query_pairs()
        .find_map(|(k, v)| (k == "error").then(|| v.into_owned()))
    {
        return Err(AuthError::new("radius_oauth", error));
    }
    url.query_pairs()
        .find_map(|(k, v)| (k == "code").then(|| v.into_owned()))
        .ok_or_else(|| AuthError::new("radius_oauth", "OAuth callback did not contain a code"))
}

async fn exchange_browser_send(
    transport: &dyn pi_ai::HttpTransport,
    gateway: &Url,
    redirect: &Url,
    code: &str,
    verifier: &str,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = browser_token_request(gateway, redirect, code, verifier)?;
    let (status, _, body) =
        pi_ai_provider_common::execute_send(transport, request, cancellation).await?;
    parse_token(status, &body, gateway)
}

async fn exchange_browser_local(
    transport: &dyn LocalHttpTransport,
    gateway: &Url,
    redirect: &Url,
    code: &str,
    verifier: &str,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = browser_token_request(gateway, redirect, code, verifier)?;
    let (status, _, body) =
        pi_ai_provider_common::execute_local(transport, request, cancellation).await?;
    parse_token(status, &body, gateway)
}

fn browser_token_request(
    gateway: &Url,
    redirect: &Url,
    code: &str,
    verifier: &str,
) -> Result<pi_ai::HttpRequest, AuthError> {
    pi_ai_provider_common::form_post(
        endpoint(gateway, "/v1/oauth/token")?.as_str(),
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect.as_str()),
            ("code", code),
            ("code_verifier", verifier),
        ],
    )
}

fn get_json(url: Url) -> pi_ai::HttpRequest {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    pi_ai::HttpRequest {
        method: http::Method::GET,
        url,
        auth_headers: HeaderMap::new(),
        headers,
        session_id: None,
        body: Vec::new(),
        timeout: None,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    }
}

fn device_event(device: &Device) -> AuthEvent {
    AuthEvent::DeviceCode {
        challenge_id: AuthChallengeId::new(device.device_code.clone()),
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval: device.interval,
        expires_in: Some(device.expires_in),
    }
}

fn radius_auth(credential: &OAuthCredential) -> Result<ResolvedAuth, AuthError> {
    let ProviderOAuthExtra::Radius { gateway_url, .. } = &credential.extra else {
        return Err(AuthError::new(
            "radius_oauth",
            "Radius OAuth credential has invalid provider metadata",
        ));
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", credential.access.expose_secret()))
            .map_err(|_| AuthError::new("radius_oauth", "invalid Radius access token"))?,
    );
    Ok(ResolvedAuth {
        api_key: None,
        headers,
        transport_headers: HeaderMap::new(),
        base_url: Some(gateway_url.clone()),
        source: AuthSource::new("OAuth"),
    })
}

fn endpoint(gateway: &Url, path: &str) -> Result<Url, AuthError> {
    gateway.join(path).map_err(|error| {
        AuthError::new(
            "radius_oauth",
            format!("invalid Radius OAuth endpoint: {error}"),
        )
    })
}
fn required_string(value: &Value, name: &str) -> Result<String, AuthError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            AuthError::new(
                "radius_oauth",
                format!("Radius OAuth token response is missing {name}"),
            )
        })
}
fn json(status: u16, body: &[u8], message: &str) -> Result<Value, AuthError> {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        Ok(value)
    } else {
        Err(oauth_response_error(
            status,
            &value,
            message,
            value.get("error").and_then(Value::as_str).unwrap_or(""),
        ))
    }
}
fn oauth_response_error(status: u16, value: &Value, message: &str, error: &str) -> AuthError {
    let description = value
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = match (error.is_empty(), description.is_empty()) {
        (false, false) => format!("{error}: {description}"),
        (false, true) => error.into(),
        (true, false) => description.into(),
        (true, true) => status.to_string(),
    };
    AuthError::new("radius_oauth", format!("{message}: {detail}"))
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
