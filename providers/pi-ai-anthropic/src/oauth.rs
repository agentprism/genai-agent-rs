//! Anthropic Claude Pro/Max OAuth using host-provided redirect reception.

use futures_util::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    AuthAnswer, AuthChallengeId, AuthError, AuthEvent, AuthHtmlPage, AuthInteraction, AuthPrompt,
    AuthSource, CancellationToken, HttpBody, HttpRequest, HttpTransport, LocalAuthInteraction,
    LocalBoxFuture, LocalHttpBody, LocalHttpTransport, LocalOAuthAuth, OAuthAuth, OAuthCredential,
    OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter, ProviderId, ProviderOAuthExtra,
    RedirectReceiverRequest, RedirectStrategy, RedirectStrategyDescription, ResolvedAuth,
    SecretString, SendBoxFuture, Timestamp, generate_pkce, parse_oauth_authorization_input,
    select_first_valid, validate_oauth_state,
};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53_692;
const CALLBACK_PATH: &str = "/callback";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const EXPIRY_SAFETY_MILLIS: i64 = 5 * 60 * 1_000;

/// Send-capable Anthropic OAuth implementation.
pub struct AnthropicOAuth {
    transport: Arc<dyn HttpTransport>,
}

impl AnthropicOAuth {
    /// Creates Anthropic OAuth around the provider's injected raw transport.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
}

impl std::fmt::Debug for AnthropicOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicOAuth")
            .finish_non_exhaustive()
    }
}

impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let pkce = generate_pkce()?;
            let (code, state) = authorize_send(
                interaction,
                &pkce.verifier,
                &pkce.challenge,
                cancellation.clone(),
            )
            .await?;
            exchange_send(
                self.transport.as_ref(),
                code,
                state,
                pkce.verifier,
                cancellation,
            )
            .await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(refresh_send(
            self.transport.as_ref(),
            credential.refresh,
            cancellation,
        ))
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move { Ok(resolved_oauth(access)) })
    }
}

/// Local-executor Anthropic OAuth implementation.
pub struct LocalAnthropicOAuth {
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalAnthropicOAuth {
    /// Creates local Anthropic OAuth around an injected raw transport.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self { transport }
    }
}

impl std::fmt::Debug for LocalAnthropicOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalAnthropicOAuth")
            .finish_non_exhaustive()
    }
}

impl LocalOAuthAuth for LocalAnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let pkce = generate_pkce()?;
            let (code, state) = authorize_local(
                interaction,
                &pkce.verifier,
                &pkce.challenge,
                cancellation.clone(),
            )
            .await?;
            exchange_local(
                self.transport.as_ref(),
                code,
                state,
                pkce.verifier,
                cancellation,
            )
            .await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(refresh_local(
            self.transport.as_ref(),
            credential.refresh,
            cancellation,
        ))
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move { Ok(resolved_oauth(access)) })
    }
}

async fn authorize_send(
    interaction: Arc<dyn AuthInteraction>,
    expected_state: &str,
    challenge: &str,
    cancellation: CancellationToken,
) -> Result<(String, String), AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let challenge_id = AuthChallengeId::new(expected_state);
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        Some(
            interaction
                .create_redirect_receiver(
                    redirect_request(challenge_id.clone()),
                    cancellation.clone(),
                )
                .await?,
        )
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities));
    }
    notify_authorization(
        interaction.as_ref(),
        challenge_id.clone(),
        challenge,
        expected_state,
    )?;

    let manual_interaction = Arc::clone(&interaction);
    let manual_challenge = challenge_id.clone();
    match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    parse_authorization(arrival.url.as_str(), expected_state)
                },
                |child| async move {
                    prompt_manual_send(manual_interaction, manual_challenge, expected_state, child)
                        .await
                },
                cancellation,
            )
            .await
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation).await?;
            parse_authorization(arrival.url.as_str(), expected_state)
        }
        (None, true) => {
            prompt_manual_send(interaction, challenge_id, expected_state, cancellation).await
        }
        (None, false) => unreachable!("unsupported capabilities were rejected"),
    }
}

async fn authorize_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    expected_state: &str,
    challenge: &str,
    cancellation: CancellationToken,
) -> Result<(String, String), AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let challenge_id = AuthChallengeId::new(expected_state);
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        Some(
            interaction
                .create_redirect_receiver(
                    redirect_request(challenge_id.clone()),
                    cancellation.clone(),
                )
                .await?,
        )
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities));
    }
    notify_local_authorization(
        interaction.as_ref(),
        challenge_id.clone(),
        challenge,
        expected_state,
    )?;

    let manual_interaction = Rc::clone(&interaction);
    let manual_challenge = challenge_id.clone();
    match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    parse_authorization(arrival.url.as_str(), expected_state)
                },
                |child| async move {
                    prompt_manual_local(manual_interaction, manual_challenge, expected_state, child)
                        .await
                },
                cancellation,
            )
            .await
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation).await?;
            parse_authorization(arrival.url.as_str(), expected_state)
        }
        (None, true) => {
            prompt_manual_local(interaction, challenge_id, expected_state, cancellation).await
        }
        (None, false) => unreachable!("unsupported capabilities were rejected"),
    }
}

fn redirect_request(challenge_id: AuthChallengeId) -> RedirectReceiverRequest {
    RedirectReceiverRequest {
        challenge_id,
        preferred: vec![RedirectStrategy::FixedLoopback {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: CALLBACK_PORT,
            path: CALLBACK_PATH.into(),
        }],
        expected_path: Some(CALLBACK_PATH.into()),
        success_page: AuthHtmlPage {
            html: "Anthropic authentication completed. You can close this window.".into(),
        },
        failure_page: AuthHtmlPage {
            html: "Anthropic authentication did not complete.".into(),
        },
    }
}

fn unsupported_redirect(capabilities: pi_ai::AuthHostCapabilities) -> AuthError {
    AuthError::UnsupportedRedirectStrategy {
        provider: ProviderId::new("anthropic"),
        required: RedirectStrategyDescription {
            required: vec![
                RedirectStrategy::FixedLoopback {
                    host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: CALLBACK_PORT,
                    path: CALLBACK_PATH.into(),
                },
                RedirectStrategy::ManualPaste,
            ],
        },
        host_capabilities: capabilities,
    }
}

fn authorization_url(challenge: &str, state: &str) -> Result<Url, AuthError> {
    let mut url = Url::parse(AUTHORIZE_URL)
        .map_err(|error| AuthError::new("anthropic_oauth", error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

fn notify_authorization(
    interaction: &dyn AuthInteraction,
    challenge_id: AuthChallengeId,
    challenge: &str,
    state: &str,
) -> Result<(), AuthError> {
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorization_url(challenge, state)?,
        instructions: Some(
            "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                .into(),
        ),
    })?;
    Ok(())
}

fn notify_local_authorization(
    interaction: &dyn LocalAuthInteraction,
    challenge_id: AuthChallengeId,
    challenge: &str,
    state: &str,
) -> Result<(), AuthError> {
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorization_url(challenge, state)?,
        instructions: Some(
            "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                .into(),
        ),
    })?;
    Ok(())
}

async fn prompt_manual_send(
    interaction: Arc<dyn AuthInteraction>,
    challenge_id: AuthChallengeId,
    expected_state: &str,
    cancellation: CancellationToken,
) -> Result<(String, String), AuthError> {
    let answer = interaction
        .prompt(
            AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the authorization code / redirect URL here:"
                    .into(),
                placeholder: Some(REDIRECT_URI.into()),
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
    parse_authorization(&input, expected_state)
}

async fn prompt_manual_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    challenge_id: AuthChallengeId,
    expected_state: &str,
    cancellation: CancellationToken,
) -> Result<(String, String), AuthError> {
    let answer = interaction
        .prompt(
            AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the authorization code / redirect URL here:"
                    .into(),
                placeholder: Some(REDIRECT_URI.into()),
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
    parse_authorization(&input, expected_state)
}

fn parse_authorization(input: &str, expected_state: &str) -> Result<(String, String), AuthError> {
    let parsed = parse_oauth_authorization_input(input);
    if let Some(state) = parsed.state.as_deref().filter(|state| !state.is_empty()) {
        validate_oauth_state(expected_state, state)?;
    }
    let code = parsed
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("anthropic_oauth", "Missing authorization code"))?;
    let state = parsed.state.unwrap_or_else(|| expected_state.to_owned());
    Ok((code, state))
}

async fn exchange_send(
    transport: &dyn HttpTransport,
    code: String,
    state: String,
    verifier: String,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = token_request(OrderedJsonObject::from_iter([
        ("grant_type", OrderedJsonValue::from("authorization_code")),
        ("client_id", OrderedJsonValue::from(CLIENT_ID)),
        ("code", OrderedJsonValue::from(code)),
        ("state", OrderedJsonValue::from(state)),
        ("redirect_uri", OrderedJsonValue::from(REDIRECT_URI)),
        ("code_verifier", OrderedJsonValue::from(verifier)),
    ]))?;
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("anthropic_oauth_exchange", error.to_string()))?;
    let body = read_send_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "exchange")
}

async fn exchange_local(
    transport: &dyn LocalHttpTransport,
    code: String,
    state: String,
    verifier: String,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = token_request(OrderedJsonObject::from_iter([
        ("grant_type", OrderedJsonValue::from("authorization_code")),
        ("client_id", OrderedJsonValue::from(CLIENT_ID)),
        ("code", OrderedJsonValue::from(code)),
        ("state", OrderedJsonValue::from(state)),
        ("redirect_uri", OrderedJsonValue::from(REDIRECT_URI)),
        ("code_verifier", OrderedJsonValue::from(verifier)),
    ]))?;
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("anthropic_oauth_exchange", error.to_string()))?;
    let body = read_local_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "exchange")
}

async fn refresh_send(
    transport: &dyn HttpTransport,
    refresh: SecretString,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = refresh_request(refresh)?;
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("anthropic_oauth_refresh", error.to_string()))?;
    let body = read_send_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "refresh")
}

async fn refresh_local(
    transport: &dyn LocalHttpTransport,
    refresh: SecretString,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = refresh_request(refresh)?;
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("anthropic_oauth_refresh", error.to_string()))?;
    let body = read_local_body(response.body, &cancellation).await?;
    parse_token_response(response.status, &body, "refresh")
}

fn refresh_request(refresh: SecretString) -> Result<HttpRequest, AuthError> {
    token_request(OrderedJsonObject::from_iter([
        ("grant_type", OrderedJsonValue::from("refresh_token")),
        ("client_id", OrderedJsonValue::from(CLIENT_ID)),
        (
            "refresh_token",
            OrderedJsonValue::from(refresh.into_secret()),
        ),
    ]))
}

fn token_request(body: OrderedJsonObject) -> Result<HttpRequest, AuthError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(HttpRequest {
        method: Method::POST,
        url: Url::parse(TOKEN_URL)
            .map_err(|error| AuthError::new("anthropic_oauth", error.to_string()))?,
        headers,
        body: OrderedJsonWriter::to_vec(&body.into()).map_err(|error| {
            AuthError::new(
                "anthropic_oauth",
                format!("failed to encode token request: {error}"),
            )
        })?,
        timeout: Some(TOKEN_TIMEOUT),
        attempt: 0,
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
            futures_util::future::Either::Right((Some(Ok(chunk)), _)) => bytes.extend(chunk),
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new("anthropic_oauth", error.to_string()));
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
            futures_util::future::Either::Right((Some(Ok(chunk)), _)) => bytes.extend(chunk),
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new("anthropic_oauth", error.to_string()));
            }
        }
    }
}

fn parse_token_response(
    status: u16,
    body: &[u8],
    operation: &str,
) -> Result<OAuthCredential, AuthError> {
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            format!("anthropic_oauth_{operation}"),
            format!("Anthropic OAuth {operation} failed with HTTP {status}"),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        AuthError::new(
            format!("anthropic_oauth_{operation}"),
            format!("Anthropic OAuth {operation} returned invalid JSON"),
        )
    })?;
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("anthropic_oauth", "response omitted access_token"))?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new("anthropic_oauth", "response omitted refresh_token"))?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| AuthError::new("anthropic_oauth", "response omitted expires_in"))?;
    let expires_millis = i64::try_from(expires_in.saturating_mul(1_000)).unwrap_or(i64::MAX);
    Ok(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(
            now_millis()
                .saturating_add(expires_millis)
                .saturating_sub(EXPIRY_SAFETY_MILLIS),
        ),
        extra: ProviderOAuthExtra::None,
    })
}

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn resolved_oauth(access: SecretString) -> ResolvedAuth {
    ResolvedAuth {
        api_key: Some(access),
        headers: HeaderMap::new(),
        base_url: None,
        source: AuthSource::new("OAuth"),
    }
}
