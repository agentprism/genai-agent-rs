//! OpenAI Codex browser and device-code OAuth over the M3.3 host contracts.

use agentprism_ai::{
    ApiKeyAuth, ApiKeyResolveRequest, AuthAnswer, AuthChallengeId, AuthError, AuthEvent,
    AuthHtmlPage, AuthInteraction, AuthPrompt, AuthSelectOption, AuthSource, CancellationToken,
    HttpBody, HttpRequest, HttpTransport, LocalApiKeyAuth, LocalApiKeyResolveRequest,
    LocalAuthInteraction, LocalBoxFuture, LocalHttpBody, LocalHttpTransport, LocalOAuthAuth,
    LocalOAuthDeviceCodePoll, LocalOAuthDeviceCodePollOptions, OAuthAuth, OAuthCredential,
    OAuthDeviceCodePoll, OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, OrderedJsonObject,
    OrderedJsonValue, OrderedJsonWriter, ProviderId, ProviderOAuthExtra, RedirectReceiverRequest,
    RedirectStrategy, RedirectStrategyDescription, ResolvedAuth, SecretString, SendBoxFuture,
    Timestamp, generate_oauth_state, generate_pkce, parse_oauth_authorization_input,
    poll_local_oauth_device_code_flow, poll_oauth_device_code_flow, select_first_valid,
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures_util::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PI_AI_RUST_USER_AGENT: &str = concat!("pi-ai-rs/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
struct DeviceInfo {
    device_auth_id: SecretString,
    user_code: String,
    interval: Duration,
}

#[derive(Clone, Debug)]
struct DeviceToken {
    authorization_code: SecretString,
    code_verifier: SecretString,
}

/// Send-capable OpenAI Codex OAuth flow.
pub struct OpenAiCodexOAuth {
    transport: Arc<dyn HttpTransport>,
}

impl OpenAiCodexOAuth {
    /// Creates the flow around an injected HTTP transport.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
}

impl std::fmt::Debug for OpenAiCodexOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCodexOAuth")
            .finish_non_exhaustive()
    }
}

impl OAuthAuth for OpenAiCodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            match select_login_send(interaction.as_ref(), cancellation.clone())
                .await?
                .as_str()
            {
                "browser" => {
                    browser_login_send(
                        Arc::clone(&interaction),
                        Arc::clone(&self.transport),
                        cancellation,
                    )
                    .await
                }
                "device_code" => {
                    device_login_send(interaction, Arc::clone(&self.transport), cancellation).await
                }
                method => Err(AuthError::new(
                    "openai_codex_oauth",
                    format!("Unknown OpenAI Codex login method: {method}"),
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
            let request = refresh_request(credential.refresh.expose_secret())?;
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| {
                    AuthError::new(
                        "openai_codex_token_refresh",
                        format!("OpenAI Codex token refresh error: {error}"),
                    )
                })?;
            let body = read_send_body(response.body, &cancellation).await?;
            parse_token_response(response.status, &body, "refresh")
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = resolved_codex_auth(credential);
        Box::pin(async move { result })
    }
}

/// Local-executor OpenAI Codex OAuth flow.
pub struct LocalOpenAiCodexOAuth {
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalOpenAiCodexOAuth {
    /// Creates the flow around an injected local HTTP transport.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self { transport }
    }
}

impl std::fmt::Debug for LocalOpenAiCodexOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalOpenAiCodexOAuth")
            .finish_non_exhaustive()
    }
}

impl LocalOAuthAuth for LocalOpenAiCodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            match select_login_local(interaction.as_ref(), cancellation.clone())
                .await?
                .as_str()
            {
                "browser" => {
                    browser_login_local(
                        Rc::clone(&interaction),
                        Rc::clone(&self.transport),
                        cancellation,
                    )
                    .await
                }
                "device_code" => {
                    device_login_local(interaction, Rc::clone(&self.transport), cancellation).await
                }
                method => Err(AuthError::new(
                    "openai_codex_oauth",
                    format!("Unknown OpenAI Codex login method: {method}"),
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
            let request = refresh_request(credential.refresh.expose_secret())?;
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| {
                    AuthError::new(
                        "openai_codex_token_refresh",
                        format!("OpenAI Codex token refresh error: {error}"),
                    )
                })?;
            let body = read_local_body(response.body, &cancellation).await?;
            parse_token_response(response.status, &body, "refresh")
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = resolved_codex_auth(credential);
        Box::pin(async move { result })
    }
}

/// Direct ChatGPT access-token authentication used by explicit request keys.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiCodexAccessTokenAuth;

impl ApiKeyAuth for OpenAiCodexAccessTokenAuth {
    fn name(&self) -> &str {
        "OpenAI Codex access token"
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let Some(token) = request.credential.and_then(|credential| credential.key) else {
                return Ok(None);
            };
            let account_id = account_id_from_jwt(token.expose_secret())?;
            resolved_codex_token(token, &account_id, "explicit_api_key").map(Some)
        })
    }
}

impl LocalApiKeyAuth for OpenAiCodexAccessTokenAuth {
    fn name(&self) -> &str {
        "OpenAI Codex access token"
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let Some(token) = request.credential.and_then(|credential| credential.key) else {
                return Ok(None);
            };
            let account_id = account_id_from_jwt(token.expose_secret())?;
            resolved_codex_token(token, &account_id, "explicit_api_key").map(Some)
        })
    }
}

fn login_prompt() -> AuthPrompt {
    AuthPrompt::Select {
        message: "Select OpenAI Codex login method:".into(),
        options: vec![
            AuthSelectOption {
                id: "browser".into(),
                label: "Browser login (default)".into(),
                description: None,
            },
            AuthSelectOption {
                id: "device_code".into(),
                label: "Device code login (headless)".into(),
                description: None,
            },
        ],
    }
}

async fn select_login_send(
    interaction: &dyn AuthInteraction,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let AuthAnswer::Selected(method) = interaction.prompt(login_prompt(), cancellation).await?
    else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "login-method prompt returned a non-selection answer",
        ));
    };
    Ok(method)
}

async fn select_login_local(
    interaction: &dyn LocalAuthInteraction,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let AuthAnswer::Selected(method) = interaction.prompt(login_prompt(), cancellation).await?
    else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "login-method prompt returned a non-selection answer",
        ));
    };
    Ok(method)
}

async fn browser_login_send(
    interaction: Arc<dyn AuthInteraction>,
    transport: Arc<dyn HttpTransport>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate_pkce()?;
    let state = generate_oauth_state()?;
    let challenge_id = AuthChallengeId::new(state.clone());
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        match interaction
            .create_redirect_receiver(redirect_request(challenge_id.clone()), cancellation.clone())
            .await
        {
            Ok(receiver) => Some(receiver),
            Err(_) if capabilities.manual_paste => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities));
    }
    // OpenAI registers the hostname form exactly. The host may bind the
    // receiver to 127.0.0.1, but authorization and exchange must continue to
    // advertise the pinned `localhost` URI.
    let redirect_uri = fixed_redirect_uri()?;
    let authorization_url = authorization_url(&pkce.challenge, &state, &redirect_uri)?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id: challenge_id.clone(),
        url: authorization_url,
        instructions: Some("A browser window should open. Complete login to finish.".into()),
    })?;
    let manual_interaction = Arc::clone(&interaction);
    let expected_state = state.clone();
    let code = match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    code_from_receiver(arrival.url.as_str(), &expected_state)
                },
                |child| prompt_manual_send(manual_interaction, challenge_id, state, child),
                cancellation.clone(),
            )
            .await?
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation.clone()).await?;
            code_from_receiver(arrival.url.as_str(), &state)?
        }
        (None, true) => {
            prompt_manual_send(interaction, challenge_id, state, cancellation.clone()).await?
        }
        (None, false) => unreachable!("unsupported capabilities were rejected"),
    };
    exchange_send(
        transport.as_ref(),
        SecretString::new(code),
        SecretString::new(pkce.verifier),
        redirect_uri.as_str(),
        cancellation,
    )
    .await
}

async fn browser_login_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    transport: Rc<dyn LocalHttpTransport>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate_pkce()?;
    let state = generate_oauth_state()?;
    let challenge_id = AuthChallengeId::new(state.clone());
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        match interaction
            .create_redirect_receiver(redirect_request(challenge_id.clone()), cancellation.clone())
            .await
        {
            Ok(receiver) => Some(receiver),
            Err(_) if capabilities.manual_paste => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities));
    }
    // Keep host ownership of callback reception while using OpenAI's exact
    // registered hostname in both OAuth requests.
    let redirect_uri = fixed_redirect_uri()?;
    let authorization_url = authorization_url(&pkce.challenge, &state, &redirect_uri)?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id: challenge_id.clone(),
        url: authorization_url,
        instructions: Some("A browser window should open. Complete login to finish.".into()),
    })?;
    let manual_interaction = Rc::clone(&interaction);
    let expected_state = state.clone();
    let code = match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    code_from_receiver(arrival.url.as_str(), &expected_state)
                },
                |child| prompt_manual_local(manual_interaction, challenge_id, state, child),
                cancellation.clone(),
            )
            .await?
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation.clone()).await?;
            code_from_receiver(arrival.url.as_str(), &state)?
        }
        (None, true) => {
            prompt_manual_local(interaction, challenge_id, state, cancellation.clone()).await?
        }
        (None, false) => unreachable!("unsupported capabilities were rejected"),
    };
    exchange_local(
        transport.as_ref(),
        SecretString::new(code),
        SecretString::new(pkce.verifier),
        redirect_uri.as_str(),
        cancellation,
    )
    .await
}

fn redirect_request(challenge_id: AuthChallengeId) -> RedirectReceiverRequest {
    RedirectReceiverRequest {
        challenge_id,
        preferred: vec![RedirectStrategy::FixedLoopback {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 1455,
            path: "/auth/callback".into(),
        }],
        expected_path: Some("/auth/callback".into()),
        success_page: AuthHtmlPage {
            html: "OpenAI authentication completed. You can close this window.".into(),
        },
        failure_page: AuthHtmlPage {
            html: "OpenAI authentication failed. Return to the application.".into(),
        },
    }
}

fn fixed_redirect_uri() -> Result<Url, AuthError> {
    Url::parse("http://localhost:1455/auth/callback").map_err(|error| {
        AuthError::new(
            "openai_codex_oauth",
            format!("invalid fixed redirect URI: {error}"),
        )
    })
}

fn unsupported_redirect(capabilities: agentprism_ai::AuthHostCapabilities) -> AuthError {
    AuthError::UnsupportedRedirectStrategy {
        provider: ProviderId::new("openai-codex"),
        required: RedirectStrategyDescription {
            required: vec![
                RedirectStrategy::FixedLoopback {
                    host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 1455,
                    path: "/auth/callback".into(),
                },
                RedirectStrategy::ManualPaste,
            ],
        },
        host_capabilities: capabilities,
    }
}

fn authorization_url(challenge: &str, state: &str, redirect_uri: &Url) -> Result<Url, AuthError> {
    let mut url = Url::parse(AUTHORIZE_URL).map_err(|error| {
        AuthError::new(
            "openai_codex_oauth",
            format!("invalid authorization URL: {error}"),
        )
    })?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "pi");
    Ok(url)
}

async fn prompt_manual_send(
    interaction: Arc<dyn AuthInteraction>,
    challenge_id: AuthChallengeId,
    state: String,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let answer = interaction.prompt(AuthPrompt::ManualCode {
        message: "Complete login in your browser, or paste the authorization code / redirect URL here:".into(),
        placeholder: Some("http://localhost:1455/auth/callback".into()),
        challenge_id,
    }, cancellation).await?;
    let AuthAnswer::Text(input) = answer else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "manual-code prompt returned a non-text answer",
        ));
    };
    code_from_manual_input(&input, &state)
}

async fn prompt_manual_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    challenge_id: AuthChallengeId,
    state: String,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let answer = interaction
        .prompt(
            AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the authorization code / redirect URL here:"
                    .into(),
                placeholder: Some("http://localhost:1455/auth/callback".into()),
                challenge_id,
            },
            cancellation,
        )
        .await?;
    let AuthAnswer::Text(input) = answer else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "manual-code prompt returned a non-text answer",
        ));
    };
    code_from_manual_input(&input, &state)
}

fn code_from_receiver(input: &str, expected_state: &str) -> Result<String, AuthError> {
    let parsed = parse_oauth_authorization_input(input);
    let state = parsed.state.as_deref().ok_or(AuthError::StateMismatch)?;
    agentprism_ai::validate_oauth_state(expected_state, state)?;
    parsed
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("openai_codex_oauth", "Missing authorization code"))
}

fn code_from_manual_input(input: &str, expected_state: &str) -> Result<String, AuthError> {
    let parsed = parse_oauth_authorization_input(input);
    if let Some(state) = parsed.state.as_deref() {
        agentprism_ai::validate_oauth_state(expected_state, state)?;
    }
    parsed
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("openai_codex_oauth", "Missing authorization code"))
}

async fn device_login_send(
    interaction: Arc<dyn AuthInteraction>,
    transport: Arc<dyn HttpTransport>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let device = start_device_send(transport.as_ref(), cancellation.clone()).await?;
    let challenge_id = AuthChallengeId::new(generate_oauth_state()?);
    interaction.notify(AuthEvent::DeviceCode {
        challenge_id,
        user_code: device.user_code.clone(),
        verification_uri: Url::parse(DEVICE_VERIFICATION_URI).expect("static URL"),
        interval: Some(device.interval),
        expires_in: Some(DEVICE_TIMEOUT),
    })?;
    let interval = device.interval;
    let mut options = OAuthDeviceCodePollOptions::new(
        Box::new(SendDevicePoll {
            transport: Arc::clone(&transport),
            device,
        }),
        cancellation.clone(),
    );
    options.interval = Some(interval);
    options.expires_in = Some(DEVICE_TIMEOUT);
    let token = poll_oauth_device_code_flow(options).await?;
    exchange_send(
        transport.as_ref(),
        token.authorization_code,
        token.code_verifier,
        DEVICE_REDIRECT_URI,
        cancellation,
    )
    .await
}

async fn device_login_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    transport: Rc<dyn LocalHttpTransport>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let device = start_device_local(transport.as_ref(), cancellation.clone()).await?;
    interaction.notify(AuthEvent::DeviceCode {
        challenge_id: AuthChallengeId::new(generate_oauth_state()?),
        user_code: device.user_code.clone(),
        verification_uri: Url::parse(DEVICE_VERIFICATION_URI).expect("static URL"),
        interval: Some(device.interval),
        expires_in: Some(DEVICE_TIMEOUT),
    })?;
    let interval = device.interval;
    let mut options = LocalOAuthDeviceCodePollOptions::new(
        Box::new(LocalDevicePoll {
            transport: Rc::clone(&transport),
            device,
        }),
        cancellation.clone(),
    );
    options.interval = Some(interval);
    options.expires_in = Some(DEVICE_TIMEOUT);
    let token = poll_local_oauth_device_code_flow(options).await?;
    exchange_local(
        transport.as_ref(),
        token.authorization_code,
        token.code_verifier,
        DEVICE_REDIRECT_URI,
        cancellation,
    )
    .await
}

struct SendDevicePoll {
    transport: Arc<dyn HttpTransport>,
    device: DeviceInfo,
}

impl OAuthDeviceCodePoll<DeviceToken> for SendDevicePoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<DeviceToken>, AuthError>> {
        Box::pin(async move {
            let request = device_poll_request(&self.device)?;
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| AuthError::new("openai_codex_device_auth", error.to_string()))?;
            let body = read_send_body(response.body, &cancellation).await?;
            parse_device_poll(response.status, &body)
        })
    }
}

struct LocalDevicePoll {
    transport: Rc<dyn LocalHttpTransport>,
    device: DeviceInfo,
}

impl LocalOAuthDeviceCodePoll<DeviceToken> for LocalDevicePoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<DeviceToken>, AuthError>> {
        Box::pin(async move {
            let request = device_poll_request(&self.device)?;
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| AuthError::new("openai_codex_device_auth", error.to_string()))?;
            let body = read_local_body(response.body, &cancellation).await?;
            parse_device_poll(response.status, &body)
        })
    }
}

async fn start_device_send(
    transport: &dyn HttpTransport,
    cancellation: CancellationToken,
) -> Result<DeviceInfo, AuthError> {
    let response = transport
        .execute(device_start_request()?, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("openai_codex_device_auth", error.to_string()))?;
    let body = read_send_body(response.body, &cancellation).await?;
    parse_device_start(response.status, &body)
}

async fn start_device_local(
    transport: &dyn LocalHttpTransport,
    cancellation: CancellationToken,
) -> Result<DeviceInfo, AuthError> {
    let response = transport
        .execute(device_start_request()?, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("openai_codex_device_auth", error.to_string()))?;
    let body = read_local_body(response.body, &cancellation).await?;
    parse_device_start(response.status, &body)
}

fn device_start_request() -> Result<HttpRequest, AuthError> {
    json_request(
        DEVICE_USER_CODE_URL,
        OrderedJsonObject::from_iter([("client_id", OrderedJsonValue::from(CLIENT_ID))]),
    )
}

fn device_poll_request(device: &DeviceInfo) -> Result<HttpRequest, AuthError> {
    json_request(
        DEVICE_TOKEN_URL,
        OrderedJsonObject::from_iter([
            (
                "device_auth_id",
                OrderedJsonValue::from(device.device_auth_id.expose_secret()),
            ),
            (
                "user_code",
                OrderedJsonValue::from(device.user_code.as_str()),
            ),
        ]),
    )
}

fn json_request(url: &str, body: OrderedJsonObject) -> Result<HttpRequest, AuthError> {
    let body = OrderedJsonWriter::to_vec(&body.into()).map_err(|error| {
        AuthError::new(
            "openai_codex_oauth",
            format!("failed to encode request: {error}"),
        )
    })?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(HttpRequest {
        method: Method::POST,
        url: Url::parse(url).map_err(|error| {
            AuthError::new("openai_codex_oauth", format!("invalid URL: {error}"))
        })?,
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body,
        timeout: Some(HTTP_TIMEOUT),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

fn parse_device_start(status: u16, body: &[u8]) -> Result<DeviceInfo, AuthError> {
    if status == 404 {
        return Err(AuthError::new(
            "openai_codex_device_auth",
            "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "openai_codex_device_auth",
            format!(
                "OpenAI Codex device code request failed with status {status}{}",
                sanitized_device_body_detail(body)
            ),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        AuthError::new(
            "openai_codex_device_auth",
            format!("Invalid OpenAI Codex device code response: {error}"),
        )
    })?;
    let device_auth_id = value
        .get("device_auth_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let interval = value
        .get("interval")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse().ok())
        })
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    match (device_auth_id, user_code, interval) {
        (Some(device_auth_id), Some(user_code), Some(interval)) => Ok(DeviceInfo {
            device_auth_id: SecretString::new(device_auth_id),
            user_code: user_code.into(),
            interval,
        }),
        _ => Err(AuthError::new(
            "openai_codex_device_auth",
            format!(
                "Invalid OpenAI Codex device code response{}",
                sanitized_device_body_detail(body)
            ),
        )),
    }
}

fn parse_device_poll(
    status: u16,
    body: &[u8],
) -> Result<OAuthDeviceCodePollResult<DeviceToken>, AuthError> {
    if (200..300).contains(&status) {
        let value: Value = serde_json::from_slice(body).map_err(|error| {
            AuthError::new(
                "openai_codex_device_auth",
                format!("Invalid OpenAI Codex device auth token response: {error}"),
            )
        })?;
        let code = value
            .get("authorization_code")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let verifier = value
            .get("code_verifier")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        return match (code, verifier) {
            (Some(code), Some(verifier)) => Ok(OAuthDeviceCodePollResult::Complete(DeviceToken {
                authorization_code: SecretString::new(code),
                code_verifier: SecretString::new(verifier),
            })),
            _ => Ok(OAuthDeviceCodePollResult::Failed {
                message: format!(
                    "Invalid OpenAI Codex device auth token response{}",
                    sanitized_device_body_detail(body)
                ),
            }),
        };
    }
    if matches!(status, 403 | 404) {
        return Ok(OAuthDeviceCodePollResult::Pending);
    }
    let error_code = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            error
                .as_str()
                .map(str::to_owned)
                .or_else(|| error.get("code").and_then(Value::as_str).map(str::to_owned))
        });
    match error_code.as_deref() {
        Some("deviceauth_authorization_pending") => Ok(OAuthDeviceCodePollResult::Pending),
        Some("slow_down") => Ok(OAuthDeviceCodePollResult::SlowDown { interval: None }),
        _ => Ok(OAuthDeviceCodePollResult::Failed {
            message: format!(
                "OpenAI Codex device auth failed with status {status}{}",
                sanitized_device_body_detail(body)
            ),
        }),
    }
}

async fn exchange_send(
    transport: &dyn HttpTransport,
    code: SecretString,
    verifier: SecretString,
    redirect_uri: &str,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let response = transport
        .execute(
            exchange_request(code.expose_secret(), verifier.expose_secret(), redirect_uri)?,
            cancellation.clone(),
        )
        .await
        .map_err(|error| AuthError::new("openai_codex_token_exchange", error.to_string()))?;
    let body = read_send_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "exchange")
}

async fn exchange_local(
    transport: &dyn LocalHttpTransport,
    code: SecretString,
    verifier: SecretString,
    redirect_uri: &str,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let response = transport
        .execute(
            exchange_request(code.expose_secret(), verifier.expose_secret(), redirect_uri)?,
            cancellation.clone(),
        )
        .await
        .map_err(|error| AuthError::new("openai_codex_token_exchange", error.to_string()))?;
    let body = read_local_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "exchange")
}

fn exchange_request(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<HttpRequest, AuthError> {
    form_request([
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ])
}

fn refresh_request(refresh: &str) -> Result<HttpRequest, AuthError> {
    form_request([
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CLIENT_ID),
    ])
}

fn form_request<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<HttpRequest, AuthError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
        .into_bytes();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    Ok(HttpRequest {
        method: Method::POST,
        url: Url::parse(TOKEN_URL).expect("static token URL"),
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body,
        timeout: Some(HTTP_TIMEOUT),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

fn parse_token_response(
    status: u16,
    body: &[u8],
    operation: &str,
) -> Result<OAuthCredential, AuthError> {
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            format!("openai_codex_token_{operation}"),
            format!(
                "OpenAI Codex token {operation} failed ({status}){}",
                redacted_token_body_detail(body)
            ),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        AuthError::new(
            format!("openai_codex_token_{operation}"),
            format!("OpenAI Codex token {operation} response missing fields: {error}"),
        )
    })?;
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires_at = value.get("expires_in").and_then(token_expiry_timestamp);
    let (access, refresh, expires_at) = match (access, refresh, expires_at) {
        (Some(access), Some(refresh), Some(expires_at)) => (access, refresh, expires_at),
        _ => {
            return Err(AuthError::new(
                format!("openai_codex_token_{operation}"),
                format!(
                    "OpenAI Codex token {operation} response missing fields{}",
                    redacted_token_body_detail(body)
                ),
            ));
        }
    };
    let account_id = account_id_from_jwt(access)?;
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at,
        extra: ProviderOAuthExtra::OpenAiCodex { account_id },
    })
}

/// Pinned Pi accepts every JSON number for `expires_in` and performs the
/// expiry arithmetic in JavaScript's IEEE-754 number domain. `Timestamp` has
/// whole-millisecond precision, so Rust performs the same floating-point
/// arithmetic, truncates the final Unix-millisecond value toward zero, and
/// saturates values outside the representable `i64` timestamp range.
fn token_expiry_timestamp(value: &Value) -> Option<Timestamp> {
    let number = value.as_number()?;
    let seconds = number
        .as_f64()
        .or_else(|| number.to_string().parse::<f64>().ok())?;
    let expires_at = now_millis() as f64 + seconds * 1_000.0;
    if expires_at.is_nan() {
        return None;
    }
    let expires_at = if expires_at >= i64::MAX as f64 {
        i64::MAX
    } else if expires_at <= i64::MIN as f64 {
        i64::MIN
    } else {
        expires_at.trunc() as i64
    };
    Some(Timestamp::from_unix_millis(expires_at))
}

fn redacted_token_body_detail(body: &[u8]) -> &'static str {
    if body.is_empty() {
        ""
    } else {
        ": [token response body redacted]"
    }
}

/// Extracts the typed ChatGPT account identifier from an OpenAI access JWT.
pub fn account_id_from_jwt(access: &str) -> Result<String, AuthError> {
    let invalid_token = || {
        AuthError::new(
            "openai_codex_account",
            "Failed to extract accountId from token",
        )
    };
    let mut parts = access.split('.');
    let (_header, payload, _signature) = match (parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature)) if parts.next().is_none() => {
            (header, payload, signature)
        }
        _ => return Err(invalid_token()),
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| STANDARD.decode(payload))
        .map_err(|_| invalid_token())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid_token())?;
    value
        .get(JWT_CLAIM_PATH)
        .and_then(Value::as_object)
        .and_then(|claims| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(invalid_token)
}

fn resolved_codex_auth(credential: &OAuthCredential) -> Result<ResolvedAuth, AuthError> {
    let ProviderOAuthExtra::OpenAiCodex { account_id } = &credential.extra else {
        return Err(AuthError::new(
            "openai_codex_account",
            "OpenAI Codex credential omits accountId",
        ));
    };
    resolved_codex_token(credential.access.clone(), account_id, "OAuth")
}

fn resolved_codex_token(
    token: SecretString,
    account_id: &str,
    source: &str,
) -> Result<ResolvedAuth, AuthError> {
    let mut headers = HeaderMap::new();
    let authorization = HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
        .map_err(|_| {
            AuthError::new(
                "openai_codex_oauth",
                "access token cannot be encoded as a header",
            )
        })?;
    let account = HeaderValue::from_str(account_id).map_err(|_| {
        AuthError::new(
            "openai_codex_oauth",
            "account ID cannot be encoded as a header",
        )
    })?;
    headers.insert(header::AUTHORIZATION, authorization);
    headers.insert("chatgpt-account-id", account);
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(PI_AI_RUST_USER_AGENT),
    );
    Ok(ResolvedAuth {
        api_key: Some(token),
        headers,
        transport_headers: HeaderMap::new(),
        environment: std::collections::BTreeMap::new(),
        base_url: None,
        source: AuthSource::new(source),
    })
}

async fn read_send_body(
    mut body: HttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, AuthError> {
    let mut bytes = Vec::new();
    loop {
        let cancelled = cancellation.cancelled().fuse();
        let next = body.next().fuse();
        futures_util::pin_mut!(cancelled, next);
        match futures_util::future::select(cancelled, next).await {
            futures_util::future::Either::Left(((), _)) => return Err(AuthError::Cancelled),
            futures_util::future::Either::Right((None, _)) => return Ok(bytes),
            futures_util::future::Either::Right((Some(Ok(chunk)), _)) => {
                bytes.extend_from_slice(&chunk)
            }
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new("openai_codex_oauth", error.to_string()));
            }
        }
    }
}

async fn read_local_body(
    mut body: LocalHttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, AuthError> {
    let mut bytes = Vec::new();
    loop {
        let cancelled = cancellation.cancelled().fuse();
        let next = body.next().fuse();
        futures_util::pin_mut!(cancelled, next);
        match futures_util::future::select(cancelled, next).await {
            futures_util::future::Either::Left(((), _)) => return Err(AuthError::Cancelled),
            futures_util::future::Either::Right((None, _)) => return Ok(bytes),
            futures_util::future::Either::Right((Some(Ok(chunk)), _)) => {
                bytes.extend_from_slice(&chunk)
            }
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new("openai_codex_oauth", error.to_string()));
            }
        }
    }
}

fn sanitized_device_body_detail(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }

    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return ": [device response body redacted]".into();
    };
    let mut secret_values = Vec::new();
    collect_device_secret_values(&value, &mut secret_values);
    let Some(safe_details) = safe_device_error_details(&value, &secret_values) else {
        return ": [device response body redacted]".into();
    };
    let serialized = serde_json::to_string(&safe_details)
        .unwrap_or_else(|_| "[device response body redacted]".into());
    format!(": {serialized}")
}

fn safe_device_error_details(value: &Value, secret_values: &[&str]) -> Option<Value> {
    let object = value.as_object()?;
    let mut safe = serde_json::Map::new();
    for (key, value) in object {
        let projected = match key.as_str() {
            "error" => safe_device_error_value(value, secret_values),
            "error_description" | "message" | "type" | "param" | "code" => {
                safe_error_scalar(value, secret_values)
            }
            _ => None,
        };
        if let Some(projected) = projected {
            safe.insert(key.clone(), projected);
        }
    }
    (!safe.is_empty()).then_some(Value::Object(safe))
}

fn safe_device_error_value(value: &Value, secret_values: &[&str]) -> Option<Value> {
    if let Some(value) = safe_error_scalar(value, secret_values) {
        return Some(value);
    }
    let object = value.as_object()?;
    let mut safe = serde_json::Map::new();
    for (key, value) in object {
        if matches!(key.as_str(), "message" | "type" | "param" | "code")
            && let Some(value) = safe_error_scalar(value, secret_values)
        {
            safe.insert(key.clone(), value);
        }
    }
    (!safe.is_empty()).then_some(Value::Object(safe))
}

fn safe_error_scalar(value: &Value, secret_values: &[&str]) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.clone()),
        Value::String(value) => Some(Value::String(redact_device_error_text(
            value,
            secret_values,
        ))),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn collect_device_secret_values<'a>(value: &'a Value, secrets: &mut Vec<&'a str>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_device_secret_key(key) {
                    if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
                        secrets.push(value);
                    }
                } else {
                    collect_device_secret_values(value, secrets);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_device_secret_values(value, secrets);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn redact_device_error_text(value: &str, secret_values: &[&str]) -> String {
    const REDACTED: &str = "[REDACTED]";
    let lowercase = value.to_ascii_lowercase();
    if DEVICE_SECRET_KEYS.iter().any(|key| lowercase.contains(key)) {
        return REDACTED.into();
    }
    let mut redacted = value.to_owned();
    for secret in secret_values
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
    {
        redacted = redacted.replace(secret, REDACTED);
    }
    redacted
}

const DEVICE_SECRET_KEYS: &[&str] = &[
    "device_auth_id",
    "user_code",
    "authorization_code",
    "code_verifier",
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "api_key",
    "password",
    "authorization",
    "cookie",
];

fn is_device_secret_key(key: &str) -> bool {
    DEVICE_SECRET_KEYS
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

fn now_millis() -> i64 {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(value).unwrap_or(i64::MAX)
}
