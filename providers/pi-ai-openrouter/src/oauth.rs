//! OpenRouter's PKCE login, which exchanges an authorization code for a
//! permanent API key while leaving redirect reception to the host.

use futures_util::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    AuthAnswer, AuthChallengeId, AuthError, AuthEvent, AuthHtmlPage, AuthInteraction, AuthPrompt,
    AuthSource, CancellationToken, HttpBody, HttpRequest, HttpTransport, LocalAuthInteraction,
    LocalBoxFuture, LocalHttpBody, LocalHttpTransport, LocalOAuthAuth, OAuthAuth, OAuthCredential,
    OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter, ProviderId, ProviderOAuthExtra,
    RedirectReceiverRequest, RedirectStrategy, RedirectStrategyDescription, ResolvedAuth,
    SecretString, SendBoxFuture, Timestamp, generate_oauth_state, generate_pkce,
    parse_oauth_authorization_input, select_first_valid,
};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const PERMANENT_EXPIRY_MILLIS: i64 = 9_007_199_254_740_991;

/// Send-capable OpenRouter OAuth implementation using an injected transport.
pub struct OpenRouterOAuth {
    transport: Arc<dyn HttpTransport>,
    login_timeout: Duration,
}

impl OpenRouterOAuth {
    /// Creates the flow around the same raw transport used by provider APIs.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            login_timeout: LOGIN_TIMEOUT,
        }
    }

    /// Replaces the five-minute authorization deadline.
    ///
    /// Hosts with virtual clocks and hermetic tests can use this to avoid a
    /// wall-clock wait; production registrations retain pinned Pi's default.
    pub fn with_login_timeout(mut self, login_timeout: Duration) -> Self {
        self.login_timeout = login_timeout;
        self
    }
}

impl std::fmt::Debug for OpenRouterOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenRouterOAuth")
            .finish_non_exhaustive()
    }
}

impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let pkce = generate_pkce()?;
            let code = authorize_send_with_deadline(
                interaction,
                &pkce.challenge,
                self.login_timeout,
                cancellation.clone(),
            )
            .await?;
            exchange_send(self.transport.as_ref(), code, pkce.verifier, cancellation).await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { Ok(credential) })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move { Ok(resolved_oauth(access)) })
    }
}

/// Local-executor OpenRouter OAuth implementation.
pub struct LocalOpenRouterOAuth {
    transport: Rc<dyn LocalHttpTransport>,
    login_timeout: Duration,
}

impl LocalOpenRouterOAuth {
    /// Creates the local flow around an injected local transport.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self {
            transport,
            login_timeout: LOGIN_TIMEOUT,
        }
    }

    /// Replaces the five-minute authorization deadline for local hosts and
    /// hermetic tests.
    pub fn with_login_timeout(mut self, login_timeout: Duration) -> Self {
        self.login_timeout = login_timeout;
        self
    }
}

impl std::fmt::Debug for LocalOpenRouterOAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalOpenRouterOAuth")
            .finish_non_exhaustive()
    }
}

impl LocalOAuthAuth for LocalOpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let pkce = generate_pkce()?;
            let code = authorize_local_with_deadline(
                interaction,
                &pkce.challenge,
                self.login_timeout,
                cancellation.clone(),
            )
            .await?;
            exchange_local(self.transport.as_ref(), code, pkce.verifier, cancellation).await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { Ok(credential) })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move { Ok(resolved_oauth(access)) })
    }
}

async fn authorize_send_with_deadline(
    interaction: Arc<dyn AuthInteraction>,
    challenge: &str,
    timeout: Duration,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let child = cancellation.child();
    let authorize = authorize_send(interaction, challenge, child.clone()).fuse();
    let cancelled = cancellation.cancelled().fuse();
    let deadline = futures_timer::Delay::new(timeout).fuse();
    futures_util::pin_mut!(authorize, cancelled, deadline);
    let result = futures_util::select_biased! {
        result = authorize => result,
        _ = cancelled => Err(AuthError::Cancelled),
        _ = deadline => Err(login_timeout_error()),
    };
    child.cancel();
    result
}

async fn authorize_local_with_deadline(
    interaction: Rc<dyn LocalAuthInteraction>,
    challenge: &str,
    timeout: Duration,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let child = cancellation.child();
    let authorize = authorize_local(interaction, challenge, child.clone()).fuse();
    let cancelled = cancellation.cancelled().fuse();
    let deadline = futures_timer::Delay::new(timeout).fuse();
    futures_util::pin_mut!(authorize, cancelled, deadline);
    let result = futures_util::select_biased! {
        result = authorize => result,
        _ = cancelled => Err(AuthError::Cancelled),
        _ = deadline => Err(login_timeout_error()),
    };
    child.cancel();
    result
}

fn login_timeout_error() -> AuthError {
    AuthError::new(
        "openrouter_oauth_timeout",
        "OpenRouter OAuth login timed out",
    )
}

async fn authorize_send(
    interaction: Arc<dyn AuthInteraction>,
    challenge: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let challenge_id = AuthChallengeId::new(generate_oauth_state()?);
    let callback_path = format!("/oauth/callback/{challenge_id}");
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        Some(
            interaction
                .create_redirect_receiver(
                    redirect_request(challenge_id.clone(), callback_path.clone()),
                    cancellation.clone(),
                )
                .await?,
        )
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities, callback_path));
    }
    let callback_url = receiver.as_ref().map_or_else(
        || manual_callback_url(&challenge_id),
        |receiver| Ok(receiver.redirect_uri().clone()),
    )?;
    notify_authorization(
        interaction.as_ref(),
        challenge_id.clone(),
        &callback_url,
        challenge,
    )?;

    let manual_interaction = Arc::clone(&interaction);
    let manual_challenge = challenge_id.clone();
    let manual_callback = callback_url.clone();
    match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    authorization_code(arrival.url.as_str())
                },
                |child| async move {
                    prompt_manual_send(manual_interaction, manual_challenge, manual_callback, child)
                        .await
                },
                cancellation,
            )
            .await
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation).await?;
            authorization_code(arrival.url.as_str())
        }
        (None, true) => {
            prompt_manual_send(interaction, challenge_id, callback_url, cancellation).await
        }
        (None, false) => unreachable!("unsupported capabilities were rejected above"),
    }
}

async fn authorize_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    challenge: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let challenge_id = AuthChallengeId::new(generate_oauth_state()?);
    let callback_path = format!("/oauth/callback/{challenge_id}");
    let capabilities = interaction.capabilities();
    let receiver = if capabilities.loopback_http {
        Some(
            interaction
                .create_redirect_receiver(
                    redirect_request(challenge_id.clone(), callback_path.clone()),
                    cancellation.clone(),
                )
                .await?,
        )
    } else {
        None
    };
    if receiver.is_none() && !capabilities.manual_paste {
        return Err(unsupported_redirect(capabilities, callback_path));
    }
    let callback_url = receiver.as_ref().map_or_else(
        || manual_callback_url(&challenge_id),
        |receiver| Ok(receiver.redirect_uri().clone()),
    )?;
    notify_local_authorization(
        interaction.as_ref(),
        challenge_id.clone(),
        &callback_url,
        challenge,
    )?;

    let manual_interaction = Rc::clone(&interaction);
    let manual_challenge = challenge_id.clone();
    let manual_callback = callback_url.clone();
    match (receiver, capabilities.manual_paste) {
        (Some(receiver), true) => {
            select_first_valid(
                |child| async move {
                    let arrival = receiver.receive(child).await?;
                    authorization_code(arrival.url.as_str())
                },
                |child| async move {
                    prompt_manual_local(
                        manual_interaction,
                        manual_challenge,
                        manual_callback,
                        child,
                    )
                    .await
                },
                cancellation,
            )
            .await
        }
        (Some(receiver), false) => {
            let arrival = receiver.receive(cancellation).await?;
            authorization_code(arrival.url.as_str())
        }
        (None, true) => {
            prompt_manual_local(interaction, challenge_id, callback_url, cancellation).await
        }
        (None, false) => unreachable!("unsupported capabilities were rejected above"),
    }
}

fn redirect_request(
    challenge_id: AuthChallengeId,
    callback_path: String,
) -> RedirectReceiverRequest {
    RedirectReceiverRequest {
        challenge_id,
        preferred: vec![RedirectStrategy::EphemeralLoopback {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            path: callback_path.clone(),
        }],
        expected_path: Some(callback_path),
        success_page: AuthHtmlPage {
            html: "Signed in to OpenRouter. You may now close this page.".into(),
        },
        failure_page: AuthHtmlPage {
            html: "OpenRouter authorization failed.".into(),
        },
    }
}

fn unsupported_redirect(
    capabilities: pi_ai::AuthHostCapabilities,
    callback_path: String,
) -> AuthError {
    AuthError::UnsupportedRedirectStrategy {
        provider: ProviderId::new("openrouter"),
        required: RedirectStrategyDescription {
            required: vec![
                RedirectStrategy::EphemeralLoopback {
                    host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    path: callback_path,
                },
                RedirectStrategy::ManualPaste,
            ],
        },
        host_capabilities: capabilities,
    }
}

fn manual_callback_url(challenge_id: &AuthChallengeId) -> Result<Url, AuthError> {
    Url::parse(&format!("http://127.0.0.1/oauth/callback/{challenge_id}")).map_err(|error| {
        AuthError::new("openrouter_oauth", format!("invalid callback URL: {error}"))
    })
}

fn authorization_url(callback_url: &Url, challenge: &str) -> Result<Url, AuthError> {
    let mut url = Url::parse(AUTHORIZE_URL).map_err(|error| {
        AuthError::new(
            "openrouter_oauth",
            format!("invalid authorization URL: {error}"),
        )
    })?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url.as_str())
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url)
}

fn notify_authorization(
    interaction: &dyn AuthInteraction,
    challenge_id: AuthChallengeId,
    callback_url: &Url,
    challenge: &str,
) -> Result<(), AuthError> {
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OpenRouter OAuth callback on {callback_url}"),
    })?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorization_url(callback_url, challenge)?,
        instructions: Some(
            "Complete sign-in in your browser. If it is on another machine, paste the final redirect URL here."
                .into(),
        ),
    })?;
    Ok(())
}

fn notify_local_authorization(
    interaction: &dyn LocalAuthInteraction,
    challenge_id: AuthChallengeId,
    callback_url: &Url,
    challenge: &str,
) -> Result<(), AuthError> {
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OpenRouter OAuth callback on {callback_url}"),
    })?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id,
        url: authorization_url(callback_url, challenge)?,
        instructions: Some(
            "Complete sign-in in your browser. If it is on another machine, paste the final redirect URL here."
                .into(),
        ),
    })?;
    Ok(())
}

async fn prompt_manual_send(
    interaction: Arc<dyn AuthInteraction>,
    challenge_id: AuthChallengeId,
    callback_url: Url,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let answer = interaction
        .prompt(
            AuthPrompt::ManualCode {
                message:
                    "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                        .into(),
                placeholder: Some(callback_url.into()),
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
    authorization_code(&input)
}

async fn prompt_manual_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    challenge_id: AuthChallengeId,
    callback_url: Url,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let answer = interaction
        .prompt(
            AuthPrompt::ManualCode {
                message:
                    "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                        .into(),
                placeholder: Some(callback_url.into()),
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
    authorization_code(&input)
}

fn authorization_code(input: &str) -> Result<String, AuthError> {
    parse_oauth_authorization_input(input)
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("openrouter_oauth", "Missing authorization code"))
}

async fn exchange_send(
    transport: &dyn HttpTransport,
    code: String,
    verifier: String,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let response = transport
        .execute(exchange_request(code, verifier)?, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("openrouter_oauth_exchange", error.to_string()))?;
    let body = read_send_body(response.body, &cancellation).await?;
    parse_exchange_response(response.status, &body)
}

async fn exchange_local(
    transport: &dyn LocalHttpTransport,
    code: String,
    verifier: String,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let response = transport
        .execute(exchange_request(code, verifier)?, cancellation.clone())
        .await
        .map_err(|error| AuthError::new("openrouter_oauth_exchange", error.to_string()))?;
    let body = read_local_body(response.body, &cancellation).await?;
    parse_exchange_response(response.status, &body)
}

fn exchange_request(code: String, verifier: String) -> Result<HttpRequest, AuthError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let body = OrderedJsonWriter::to_vec(
        &OrderedJsonObject::from_iter([
            ("code", OrderedJsonValue::from(code)),
            ("code_verifier", OrderedJsonValue::from(verifier)),
            ("code_challenge_method", OrderedJsonValue::from("S256")),
        ])
        .into(),
    )
    .map_err(|error| {
        AuthError::new(
            "openrouter_oauth_exchange",
            format!("failed to encode token exchange: {error}"),
        )
    })?;
    Ok(HttpRequest {
        method: Method::POST,
        url: Url::parse(TOKEN_URL).map_err(|error| {
            AuthError::new(
                "openrouter_oauth_exchange",
                format!("invalid token URL: {error}"),
            )
        })?,
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body,
        timeout: Some(TOKEN_EXCHANGE_TIMEOUT),
        transport: None,
        websocket_connect_timeout: None,
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
            futures_util::future::Either::Right((Some(Ok(chunk)), _)) => {
                bytes.extend_from_slice(&chunk);
            }
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new(
                    "openrouter_oauth_exchange",
                    error.to_string(),
                ));
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
                bytes.extend_from_slice(&chunk);
            }
            futures_util::future::Either::Right((Some(Err(error)), _)) => {
                return Err(AuthError::new(
                    "openrouter_oauth_exchange",
                    error.to_string(),
                ));
            }
        }
    }
}

fn parse_exchange_response(status: u16, body: &[u8]) -> Result<OAuthCredential, AuthError> {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    if !(200..300).contains(&status) {
        let detail = parsed.as_ref().and_then(exchange_error_detail);
        return Err(AuthError::new(
            "openrouter_oauth_exchange",
            format!(
                "OpenRouter OAuth key exchange failed (HTTP {status}){}",
                detail.map_or_else(String::new, |detail| format!(": {detail}"))
            ),
        ));
    }
    let key = parsed
        .as_ref()
        .and_then(|value| value.get("key"))
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            AuthError::new(
                "openrouter_oauth_exchange",
                "OpenRouter OAuth response carries no \"key\"",
            )
        })?;
    Ok(OAuthCredential {
        access: SecretString::new(key),
        refresh: SecretString::new(""),
        expires_at: Timestamp::from_unix_millis(PERMANENT_EXPIRY_MILLIS),
        extra: ProviderOAuthExtra::None,
    })
}

fn exchange_error_detail(body: &Value) -> Option<&str> {
    body.get("error_description")
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .or_else(|| body.get("error").and_then(Value::as_str))
        .or_else(|| {
            body.get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
}

fn resolved_oauth(access: SecretString) -> ResolvedAuth {
    ResolvedAuth {
        api_key: Some(access),
        headers: HeaderMap::new(),
        transport_headers: HeaderMap::new(),
        base_url: None,
        source: AuthSource::new("OAuth"),
    }
}
