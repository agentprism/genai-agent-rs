use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::oauth_page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;
use super::{
    OAuthHttpError, now_millis, read_loopback_request, request, send_http, write_loopback_response,
};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, AuthPrompt, AuthSelectOption, ModelAuth, OAuthAuth,
    OAuthCredential, OAuthCredentialType, ProviderAuthInteraction,
};
use crate::types::{FetchFunction, default_fetch};
use crate::utils::abort::{AbortController, AbortReason};
use crate::utils::provider_env::get_provider_env_value;
use base64::prelude::{BASE64_STANDARD, Engine as _};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const OPENAI_CODEX_BROWSER_LOGIN_METHOD: &str = "browser";
const OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Clone)]
struct Endpoints {
    authorize_url: String,
    token_url: String,
    device_user_code_url: String,
    device_token_url: String,
}

struct OAuthToken {
    access: String,
    refresh: String,
    expires: f64,
}

struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: f64,
}

#[derive(Debug)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

struct AuthorizationInput {
    code: Option<String>,
    state: Option<String>,
}

type LocalCallbackSender = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

struct LocalOAuthServer {
    receiver: Option<oneshot::Receiver<Option<String>>>,
    settle: LocalCallbackSender,
    shutdown: watch::Sender<bool>,
}

impl LocalOAuthServer {
    fn cancel_wait(&self) {
        if let Some(settle) = self.settle.lock().expect("callback mutex").take() {
            let _ = settle.send(None);
        }
    }

    async fn wait_for_code(&mut self) -> Option<String> {
        self.receiver
            .take()
            .expect("callback wait called once")
            .await
            .unwrap_or(None)
    }

    fn close(&self) {
        self.cancel_wait();
        self.shutdown.send_replace(true);
    }
}

#[cfg(test)]
fn pending_local_oauth_server() -> LocalOAuthServer {
    let (settle_sender, receiver) = oneshot::channel();
    let (shutdown, _) = watch::channel(false);
    LocalOAuthServer {
        receiver: Some(receiver),
        settle: Arc::new(Mutex::new(Some(settle_sender))),
        shutdown,
    }
}

fn endpoints() -> Endpoints {
    Endpoints {
        authorize_url: format!("{AUTH_BASE_URL}/oauth/authorize"),
        token_url: format!("{AUTH_BASE_URL}/oauth/token"),
        device_user_code_url: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/usercode"),
        device_token_url: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/token"),
    }
}

fn create_state() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AuthError::new(format!("Could not generate OAuth state: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput {
            code: None,
            state: None,
        };
    }
    if let Ok(url) = url::Url::parse(value) {
        return AuthorizationInput {
            code: url
                .query_pairs()
                .find(|(name, _)| name == "code")
                .map(|(_, value)| value.into_owned()),
            state: url
                .query_pairs()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.into_owned()),
        };
    }
    if let Some((code, state)) = value.split_once('#') {
        return AuthorizationInput {
            code: Some(code.to_owned()),
            state: Some(state.to_owned()),
        };
    }
    if value.contains("code=") {
        let pairs =
            url::form_urlencoded::parse(value.strip_prefix('?').unwrap_or(value).as_bytes())
                .collect::<Vec<_>>();
        return AuthorizationInput {
            code: pairs
                .iter()
                .find(|(name, _)| name == "code")
                .map(|(_, value)| value.to_string()),
            state: pairs
                .iter()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.to_string()),
        };
    }
    AuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

fn parse_manual_authorization_input(
    input: &str,
    expected_state: &str,
) -> Result<String, AuthError> {
    let parsed = parse_authorization_input(input);
    if parsed
        .state
        .as_deref()
        .filter(|candidate| !candidate.is_empty())
        .is_some_and(|candidate| candidate != expected_state)
    {
        return Err(AuthError::new("State mismatch"));
    }
    parsed
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("Missing authorization code"))
}

async fn wait_for_browser_code(
    server: &mut LocalOAuthServer,
    prompt: AuthFuture<String>,
    signal: Arc<dyn crate::types::AbortSignal>,
    expected_state: &str,
) -> Result<String, AuthError> {
    tokio::pin!(prompt);
    tokio::select! {
        biased;
        callback = server.wait_for_code() => {
            if let Some(code) = callback {
                Ok(code)
            } else {
                parse_manual_authorization_input(&prompt.await?, expected_state)
            }
        }
        input = &mut prompt => {
            server.cancel_wait();
            parse_manual_authorization_input(&input?, expected_state)
        }
        _ = signal.cancelled() => {
            server.cancel_wait();
            parse_manual_authorization_input(&prompt.await?, expected_state)
        }
    }
}

fn decode_jwt(token: &str) -> Option<Value> {
    let payload = token.split('.').collect::<Vec<_>>();
    if payload.len() != 3 {
        return None;
    }
    let mut encoded = payload[1].to_owned();
    while !encoded.len().is_multiple_of(4) {
        encoded.push('=');
    }
    let decoded = BASE64_STANDARD.decode(encoded).ok()?;
    serde_json::from_slice(&decoded).ok()
}

async fn send(
    fetch: Arc<dyn FetchFunction>,
    request: crate::types::ProviderHttpRequest,
) -> Result<super::OAuthHttpResponse, AuthError> {
    send_http(fetch, request.clone(), None)
        .await
        .map_err(|error| match error {
            OAuthHttpError::Aborted => AuthError::new("Login cancelled"),
            other => AuthError::new(other.to_string()),
        })
}

fn read_token_response(
    response: super::OAuthHttpResponse,
    operation: &str,
) -> Result<OAuthToken, AuthError> {
    if !response.ok() {
        let detail = if response.body.is_empty() {
            response.status_text
        } else {
            response.body
        };
        return Err(AuthError::new(format!(
            "OpenAI Codex token {operation} failed ({}): {detail}",
            response.status
        )));
    }
    let json = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| AuthError::new(error.to_string()))?;
    let access = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let refresh = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires_in = json.get("expires_in").and_then(Value::as_f64);
    let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
        return Err(AuthError::new(format!(
            "OpenAI Codex token {operation} response missing fields: {json}"
        )));
    };
    Ok(OAuthToken {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now_millis() + expires_in * 1_000.0,
    })
}

async fn exchange_authorization_code(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    code: String,
    verifier: String,
    redirect_uri: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthToken, AuthError> {
    let response = send(
        fetch,
        request(
            "POST",
            token_url,
            [("Content-Type", "application/x-www-form-urlencoded")],
            super::form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &code),
                ("code_verifier", &verifier),
                ("redirect_uri", &redirect_uri),
            ]),
            signal,
        ),
    )
    .await?;
    read_token_response(response, "exchange")
}

async fn refresh_access_token(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    refresh_token: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthToken, AuthError> {
    let response = send_http(
        fetch,
        request(
            "POST",
            token_url,
            [("Content-Type", "application/x-www-form-urlencoded")],
            super::form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", CLIENT_ID),
            ]),
            signal,
        ),
        None,
    )
    .await
    .map_err(|error| AuthError::new(format!("OpenAI Codex token refresh error: {error}")))?;
    read_token_response(response, "refresh")
}

fn javascript_number(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return Some(0.0);
    }
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    if unsigned == "Infinity" {
        return Some(f64::INFINITY);
    }
    if value == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    if value.starts_with('+') || value.starts_with('-') {
        return value.parse().ok();
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix)
                .ok()
                .map(|number| number as f64);
        }
    }
    value.parse().ok()
}

async fn start_device_auth(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<DeviceAuthInfo, AuthError> {
    let response = send(
        fetch,
        request(
            "POST",
            endpoints.device_user_code_url,
            [("Content-Type", "application/json")],
            serde_json::to_vec(&json!({ "client_id": CLIENT_ID })).expect("static JSON"),
            signal,
        ),
    )
    .await?;
    if !response.ok() {
        if response.status == 404 {
            return Err(AuthError::new(
                "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
            ));
        }
        return Err(AuthError::new(format!(
            "OpenAI Codex device code request failed with status {}{}",
            response.status,
            if response.body.is_empty() {
                String::new()
            } else {
                format!(": {}", response.body)
            }
        )));
    }
    let json = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| AuthError::new(error.to_string()))?;
    let interval_seconds = json.get("interval").and_then(|interval| match interval {
        Value::String(value) => javascript_number(value),
        _ => interval.as_f64(),
    });
    let device_auth_id = json
        .get("device_auth_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let user_code = json
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let (Some(device_auth_id), Some(user_code), Some(interval_seconds)) =
        (device_auth_id, user_code, interval_seconds)
    else {
        return Err(AuthError::new(format!(
            "Invalid OpenAI Codex device code response: {json}"
        )));
    };
    if !interval_seconds.is_finite() || interval_seconds < 0.0 {
        return Err(AuthError::new(format!(
            "Invalid OpenAI Codex device code response: {json}"
        )));
    }
    Ok(DeviceAuthInfo {
        device_auth_id: device_auth_id.to_owned(),
        user_code: user_code.to_owned(),
        interval_seconds,
    })
}

async fn poll_device_auth(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    device: DeviceAuthInfo,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<DeviceTokenSuccess, AuthError> {
    poll_device_auth_with_timeout(
        fetch,
        endpoints,
        device,
        signal,
        DEVICE_CODE_TIMEOUT_SECONDS,
    )
    .await
}

async fn poll_device_auth_with_timeout(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    device: DeviceAuthInfo,
    signal: Arc<dyn crate::types::AbortSignal>,
    timeout_seconds: f64,
) -> Result<DeviceTokenSuccess, AuthError> {
    let poll_signal = signal.clone();
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(timeout_seconds),
            wait_before_first_poll: false,
            signal,
        },
        move || {
            let fetch = fetch.clone();
            let url = endpoints.device_token_url.clone();
            let device_auth_id = device.device_auth_id.clone();
            let user_code = device.user_code.clone();
            let signal = poll_signal.clone();
            async move {
                let response = send(
                    fetch,
                    request(
                        "POST",
                        url,
                        [("Content-Type", "application/json")],
                        serde_json::to_vec(&json!({
                            "device_auth_id": device_auth_id,
                            "user_code": user_code,
                        }))
                        .expect("serializable JSON"),
                        signal,
                    ),
                )
                .await?;
                if response.ok() {
                    let json = serde_json::from_str::<Value>(&response.body)
                        .map_err(|error| AuthError::new(error.to_string()))?;
                    let authorization_code = json
                        .get("authorization_code")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty());
                    let code_verifier = json
                        .get("code_verifier")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty());
                    return Ok(match (authorization_code, code_verifier) {
                        (Some(authorization_code), Some(code_verifier)) => {
                            OAuthDeviceCodePollResult::Complete(DeviceTokenSuccess {
                                authorization_code: authorization_code.to_owned(),
                                code_verifier: code_verifier.to_owned(),
                            })
                        }
                        _ => OAuthDeviceCodePollResult::Failed {
                            message: format!(
                                "Invalid OpenAI Codex device auth token response: {json}"
                            ),
                        },
                    });
                }
                if response.status == 403 || response.status == 404 {
                    return Ok(OAuthDeviceCodePollResult::Pending);
                }
                let error_code = serde_json::from_str::<Value>(&response.body)
                    .ok()
                    .and_then(|json| json.get("error").cloned())
                    .and_then(|error| match error {
                        Value::String(code) => Some(code),
                        Value::Object(error) => {
                            error.get("code").and_then(Value::as_str).map(str::to_owned)
                        }
                        _ => None,
                    });
                Ok(match error_code.as_deref() {
                    Some("deviceauth_authorization_pending") => OAuthDeviceCodePollResult::Pending,
                    Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: None,
                    },
                    _ => OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "OpenAI Codex device auth failed with status {}{}",
                            response.status,
                            if response.body.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", response.body)
                            }
                        ),
                    },
                })
            }
        },
    )
    .await
}

fn create_authorization_flow(
    endpoints: &Endpoints,
    originator: &str,
) -> Result<(String, String, String), AuthError> {
    let pkce = generate_pkce()?;
    let state = create_state()?;
    let mut url = url::Url::parse(&endpoints.authorize_url)
        .map_err(|error| AuthError::new(error.to_string()))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", originator),
    ]);
    Ok((pkce.verifier, state, url.to_string()))
}

async fn handle_browser_callback_connection(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    expected_state: &str,
    settle: &LocalCallbackSender,
) {
    let Ok(request) = read_loopback_request(stream).await else {
        return;
    };
    let Ok(url) = url::Url::parse(&format!("http://localhost{}", request.target)) else {
        let _ = write_loopback_response(
            stream,
            500,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("Internal error while processing OAuth callback.", None),
        )
        .await;
        return;
    };
    if url.path() != "/auth/callback" {
        let _ = write_loopback_response(
            stream,
            404,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("Callback route not found.", None),
        )
        .await;
        return;
    }
    let callback_state = url
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned());
    if callback_state.as_deref() != Some(expected_state) {
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("State mismatch.", None),
        )
        .await;
        return;
    }
    let code = url
        .query_pairs()
        .find(|(name, _)| name == "code")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let Some(code) = code else {
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("Missing authorization code.", None),
        )
        .await;
        return;
    };
    let _ = write_loopback_response(
        stream,
        200,
        "text/html; charset=utf-8",
        None,
        &oauth_success_html("OpenAI authentication completed. You can close this window."),
    )
    .await;
    if let Some(settle) = settle.lock().expect("callback mutex").take() {
        let _ = settle.send(Some(code));
    }
}

async fn start_local_oauth_server(state: String, host: String) -> LocalOAuthServer {
    let (settle_sender, receiver) = oneshot::channel();
    let settle = Arc::new(Mutex::new(Some(settle_sender)));
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let listener = match TcpListener::bind((host.as_str(), 1_455)).await {
        Ok(listener) => listener,
        Err(_) => {
            if let Some(settle) = settle.lock().expect("callback mutex").take() {
                let _ = settle.send(None);
            }
            return LocalOAuthServer {
                receiver: Some(receiver),
                settle,
                shutdown,
            };
        }
    };
    let task_settle = settle.clone();
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                biased;
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() || *shutdown_receiver.borrow() { break; }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                break;
            };
            handle_browser_callback_connection(&mut stream, &state, &task_settle).await;
        }
    });
    LocalOAuthServer {
        receiver: Some(receiver),
        settle,
        shutdown,
    }
}

fn get_account_id(access_token: &str) -> Option<String> {
    decode_jwt(access_token)?
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn credentials_from_token(token: OAuthToken) -> Result<OAuthCredential, AuthError> {
    let account_id = get_account_id(&token.access)
        .ok_or_else(|| AuthError::new("Failed to extract accountId from token"))?;
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        access: token.access,
        refresh: token.refresh,
        expires: token.expires,
        extra: Map::from_iter([("accountId".to_owned(), Value::String(account_id))]),
    })
}

async fn exchange_for_credentials(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    code: String,
    verifier: String,
    redirect_uri: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    credentials_from_token(
        exchange_authorization_code(
            fetch,
            endpoints.token_url,
            code,
            verifier,
            redirect_uri,
            signal,
        )
        .await?,
    )
}

async fn login_device_code(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let device =
        start_device_auth(fetch.clone(), endpoints.clone(), interaction.signal.clone()).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.to_owned(),
        interval_seconds: Some(device.interval_seconds),
        expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
    });
    let code = poll_device_auth(
        fetch.clone(),
        endpoints.clone(),
        device,
        interaction.signal.clone(),
    )
    .await?;
    exchange_for_credentials(
        fetch,
        endpoints,
        code.authorization_code,
        code.code_verifier,
        DEVICE_REDIRECT_URI.to_owned(),
        interaction.signal,
    )
    .await
}

async fn login_browser(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    callback_host: Option<String>,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let (verifier, state, url) = create_authorization_flow(&endpoints, "pi")?;
    let host = callback_host.unwrap_or_else(|| {
        get_provider_env_value("PI_OAUTH_CALLBACK_HOST", None)
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_owned())
    });
    let mut server = start_local_oauth_server(state.clone(), host).await;
    let manual_abort = AbortController::new();
    interaction.interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some("A browser window should open. Complete login to finish.".to_owned()),
    });
    let prompt = interaction.interaction.prompt(AuthPrompt::ManualCode {
        message:
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                .to_owned(),
        placeholder: Some(REDIRECT_URI.to_owned()),
        signal: Some(manual_abort.signal()),
    });
    let result =
        wait_for_browser_code(&mut server, prompt, interaction.signal.clone(), &state).await;
    let credential = match result {
        Ok(code) => {
            exchange_for_credentials(
                fetch,
                endpoints,
                code,
                verifier,
                REDIRECT_URI.to_owned(),
                interaction.signal,
            )
            .await
        }
        Err(error) => Err(error),
    };
    manual_abort.abort(AbortReason::default_abort());
    server.close();
    credential
}

fn openai_codex_oauth_with(
    fetch: Arc<dyn FetchFunction>,
    endpoints: Endpoints,
    callback_host: Option<String>,
) -> OAuthAuth {
    let login_fetch = fetch.clone();
    let login_endpoints = endpoints.clone();
    OAuthAuth {
        name: "OpenAI (ChatGPT Plus/Pro)".to_owned(),
        is_subscription: Some(true),
        login_label: None,
        login: Arc::new(move |interaction| {
            let fetch = login_fetch.clone();
            let endpoints = login_endpoints.clone();
            let callback_host = callback_host.clone();
            Box::pin(async move {
                let method = interaction
                    .interaction
                    .prompt(AuthPrompt::Select {
                        message: "Select OpenAI Codex login method:".to_owned(),
                        options: vec![
                            AuthSelectOption {
                                id: OPENAI_CODEX_BROWSER_LOGIN_METHOD.to_owned(),
                                label: "Browser login (default)".to_owned(),
                                description: None,
                            },
                            AuthSelectOption {
                                id: OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.to_owned(),
                                label: "Device code login (headless)".to_owned(),
                                description: None,
                            },
                        ],
                        signal: None,
                    })
                    .await?;
                match method.as_str() {
                    OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD => {
                        login_device_code(fetch, endpoints, interaction).await
                    }
                    OPENAI_CODEX_BROWSER_LOGIN_METHOD => {
                        login_browser(fetch, endpoints, callback_host, interaction).await
                    }
                    _ => Err(AuthError::new(format!(
                        "Unknown OpenAI Codex login method: {method}"
                    ))),
                }
            }) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            let fetch = fetch.clone();
            let token_url = endpoints.token_url.clone();
            Box::pin(async move {
                credentials_from_token(
                    refresh_access_token(fetch, token_url, credential.refresh, signal).await?,
                )
            }) as AuthFuture<OAuthCredential>
        }),
        to_auth: Arc::new(|credential| {
            Box::pin(async move {
                Ok(ModelAuth {
                    api_key: Some(credential.access),
                    ..ModelAuth::default()
                })
            })
        }),
    }
}

pub fn openai_codex_oauth() -> OAuthAuth {
    openai_codex_oauth_with(default_fetch(), endpoints(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::AuthInteraction;
    use crate::types::ProviderHttpRequest;
    use crate::utils::abort::{AbortController, AbortReason};
    use base64::prelude::BASE64_STANDARD_NO_PAD;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    async fn browser_callback_request(target: &str, settle: LocalCallbackSender) -> String {
        let (mut client, mut server) = tokio::io::duplex(16_384);
        client
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("request");
        let task = tokio::spawn(async move {
            handle_browser_callback_connection(&mut server, "expected-state", &settle).await;
        });
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("response");
        task.await.expect("handler");
        response
    }

    struct DeviceInteraction {
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<Arc<dyn crate::types::AbortSignal>>,
    }

    impl AuthInteraction for DeviceInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            self.signal.clone()
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            let AuthPrompt::Select {
                message, options, ..
            } = prompt
            else {
                panic!("expected select prompt")
            };
            assert_eq!(message, "Select OpenAI Codex login method:");
            assert_eq!(
                options
                    .iter()
                    .map(|option| (option.id.as_str(), option.label.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("browser", "Browser login (default)"),
                    ("device_code", "Device code login (headless)")
                ]
            );
            Box::pin(async { Ok(OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.to_owned()) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events").push(event);
        }
    }

    struct CancelledSelection;

    impl AuthInteraction for CancelledSelection {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            assert!(matches!(prompt, AuthPrompt::Select { .. }));
            Box::pin(async { Err(AuthError::new("Login cancelled")) })
        }

        fn notify(&self, _event: AuthEvent) {}
    }

    fn access_token(account_id: &str) -> String {
        let header = BASE64_STANDARD_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = BASE64_STANDARD_NO_PAD.encode(
            serde_json::to_vec(&json!({ JWT_CLAIM_PATH: { "chatgpt_account_id": account_id } }))
                .expect("JSON"),
        );
        format!("{header}.{payload}.signature")
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:77` and `:180`.
    #[tokio::test(start_paused = true)]
    async fn device_code_flow_polls_immediately_and_extracts_account_id() {
        let token = access_token("account-123");
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                r#"{"device_auth_id":"device-auth-id","user_code":"ABCD-1234","interval":"5"}"#,
            ),
            response(403, r#"{"error":{"code":"deviceauth_authorization_pending"}}"#),
            response(
                200,
                r#"{"authorization_code":"oauth-code","code_verifier":"device-code-verifier"}"#,
            ),
            response(
                200,
                json!({ "access_token": token, "refresh_token": "refresh-token", "expires_in": 3600 }).to_string(),
            ),
        ])));
        let requests = Arc::new(Mutex::new(Vec::<ProviderHttpRequest>::new()));
        let fetcher = {
            let replies = replies.clone();
            let requests = requests.clone();
            fetch(move |request| {
                requests.lock().expect("requests").push(request);
                Ok(replies.lock().expect("replies").pop_front().expect("reply"))
            })
        };
        let interaction = Arc::new(DeviceInteraction {
            events: Mutex::new(Vec::new()),
            signal: None,
        });
        let login = (openai_codex_oauth_with(fetcher, endpoints(), None).login)(
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        );
        let task = tokio::spawn(login);
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let credential = task.await.expect("task").expect("credential");
        assert_eq!(credential.extra["accountId"], "account-123");
        assert!(matches!(
            interaction.events.lock().expect("events").as_slice(),
            [AuthEvent::DeviceCode { user_code, verification_uri, interval_seconds: Some(5.0), expires_in_seconds: Some(900.0) }]
                if user_code == "ABCD-1234" && verification_uri == DEVICE_VERIFICATION_URI
        ));
        let requests = requests.lock().expect("requests");
        assert_eq!(
            requests
                .iter()
                .map(|request| request.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://auth.openai.com/api/accounts/deviceauth/usercode",
                "https://auth.openai.com/api/accounts/deviceauth/token",
                "https://auth.openai.com/api/accounts/deviceauth/token",
                "https://auth.openai.com/oauth/token",
            ]
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                requests[0].body.as_deref().expect("device request body"),
            )
            .expect("device request"),
            json!({ "client_id": CLIENT_ID })
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                requests[1].body.as_deref().expect("poll request body"),
            )
            .expect("poll request"),
            json!({ "device_auth_id": "device-auth-id", "user_code": "ABCD-1234" })
        );
        let exchange =
            url::form_urlencoded::parse(requests[3].body.as_deref().expect("exchange body"))
                .collect::<HashMap<_, _>>();
        assert_eq!(
            exchange.get("code").map(|value| value.as_ref()),
            Some("oauth-code")
        );
        assert_eq!(
            exchange.get("code_verifier").map(|value| value.as_ref()),
            Some("device-code-verifier")
        );
        assert_eq!(
            exchange.get("redirect_uri").map(|value| value.as_ref()),
            Some(DEVICE_REDIRECT_URI)
        );
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:266`.
    #[tokio::test]
    async fn cancelled_login_method_selection_is_propagated() {
        let error = (openai_codex_oauth_with(default_fetch(), endpoints(), None).login)(
            crate::auth::helpers::normalize_interaction(Arc::new(CancelledSelection)),
        )
        .await
        .expect_err("cancelled selection");
        assert_eq!(error.message, "Login cancelled");
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:278`.
    #[tokio::test]
    async fn device_code_flow_cancels_while_waiting() {
        let controller = AbortController::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = {
            let calls = calls.clone();
            fetch(move |_| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                Ok(if call == 0 {
                    response(
                        200,
                        r#"{"device_auth_id":"device-auth-id","user_code":"ABCD-1234","interval":"5"}"#,
                    )
                } else {
                    response(
                        403,
                        r#"{"error":{"code":"deviceauth_authorization_pending"}}"#,
                    )
                })
            })
        };
        let interaction = Arc::new(DeviceInteraction {
            events: Mutex::new(Vec::new()),
            signal: Some(controller.signal()),
        });
        let login = (openai_codex_oauth_with(fetcher, endpoints(), None).login)(
            crate::auth::helpers::normalize_interaction(interaction),
        );
        let task = tokio::spawn(login);
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        controller.abort(AbortReason::default_abort());
        assert_eq!(
            task.await.expect("task").expect_err("cancelled").message,
            "Login cancelled"
        );
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:323`.
    #[tokio::test]
    async fn device_code_flow_times_out_after_fifteen_minutes() {
        assert_eq!(DEVICE_CODE_TIMEOUT_SECONDS, 15.0 * 60.0);
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = {
            let calls = calls.clone();
            fetch(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(response(
                    403,
                    r#"{"error":{"code":"deviceauth_authorization_pending"}}"#,
                ))
            })
        };
        let error = poll_device_auth_with_timeout(
            fetcher,
            endpoints(),
            DeviceAuthInfo {
                device_auth_id: "device-auth-id".to_owned(),
                user_code: "ABCD-1234".to_owned(),
                interval_seconds: 60.0,
            },
            AbortController::new().signal(),
            0.01,
        )
        .await
        .expect_err("timeout");
        assert_eq!(error.message, "Device flow timed out");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Pins pi `src/auth/oauth/openai-codex.ts:73-101` and `:483-499`.
    #[test]
    fn manual_input_uses_javascript_empty_state_and_search_param_semantics() {
        assert_eq!(
            parse_manual_authorization_input("authorization-code#", "expected-state")
                .expect("empty state is not a mismatch"),
            "authorization-code"
        );
        assert_eq!(
            parse_manual_authorization_input(
                "http://localhost/callback?code=authorization-code&state=",
                "expected-state",
            )
            .expect("empty URL state is not a mismatch"),
            "authorization-code"
        );
        assert_eq!(
            parse_manual_authorization_input("?code=authorization-code", "expected-state")
                .expect("leading question mark"),
            "authorization-code"
        );
    }

    /// Pins pi `src/auth/oauth/openai-codex.ts:449-499`.
    #[tokio::test]
    async fn browser_abort_cancels_callback_wait_but_not_manual_prompt() {
        let controller = AbortController::new();
        let (send_input, receive_input) = oneshot::channel();
        let prompt: AuthFuture<String> = Box::pin(async move {
            receive_input
                .await
                .map_err(|_| AuthError::new("manual prompt closed"))
        });
        let signal = controller.signal();
        let mut task = tokio::spawn(async move {
            let mut server = pending_local_oauth_server();
            wait_for_browser_code(&mut server, prompt, signal, "expected-state").await
        });
        tokio::task::yield_now().await;
        controller.abort(AbortReason::default_abort());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut task)
                .await
                .is_err()
        );
        assert!(send_input.send("authorization-code#".to_owned()).is_ok());
        assert_eq!(
            task.await.expect("task").expect("manual code"),
            "authorization-code"
        );
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:366`.
    #[tokio::test(start_paused = true)]
    async fn device_poll_treats_403_and_404_as_pending() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(403, r#"{"error":"access_denied"}"#),
            response(404, "not ready"),
            response(
                200,
                r#"{"authorization_code":"code","code_verifier":"verifier"}"#,
            ),
        ])));
        let fetcher = {
            let replies = replies.clone();
            fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
        };
        let task = tokio::spawn(poll_device_auth(
            fetcher,
            endpoints(),
            DeviceAuthInfo {
                device_auth_id: "id".to_owned(),
                user_code: "code".to_owned(),
                interval_seconds: 1.0,
            },
            AbortController::new().signal(),
        ));
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            task.await.expect("task").expect("token").authorization_code,
            "code"
        );
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:456`.
    #[tokio::test]
    async fn refresh_failure_is_returned_without_side_effect_logging() {
        let fetcher = fetch(|_| {
            Ok(response(
                401,
                r#"{"error":{"message":"Could not validate your token."}}"#,
            ))
        });
        let oauth = openai_codex_oauth_with(fetcher, endpoints(), None);
        let error = (oauth.refresh)(
            OAuthCredential {
                kind: OAuthCredentialType::OAuth,
                access: "bad".to_owned(),
                refresh: "bad".to_owned(),
                expires: 0.0,
                extra: Map::new(),
            },
            AbortController::new().signal(),
        )
        .await
        .expect_err("refresh");
        assert!(
            error
                .message
                .contains("OpenAI Codex token refresh failed (401)")
        );
        assert!(error.message.contains("Could not validate your token"));
    }

    /// Pins pi `src/auth/oauth/openai-codex.ts:286-306`.
    #[test]
    fn browser_authorize_url_carries_all_codex_flags() {
        let (_, _, url) = create_authorization_flow(&endpoints(), "pi").expect("flow");
        let url = url::Url::parse(&url).expect("URL");
        let pairs = url.query_pairs().collect::<Vec<_>>();
        for (name, value) in [
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "pi"),
        ] {
            assert!(pairs.iter().any(|pair| pair.0 == name && pair.1 == value));
        }
    }

    /// Ports pi `test/openai-codex-oauth.test.ts:428`.
    #[tokio::test]
    async fn device_poll_failure_includes_the_response_body() {
        let error = poll_device_auth(
            fetch(|_| {
                Ok(response(
                    500,
                    r#"{"error":"server_error","error_description":"try again later"}"#,
                ))
            }),
            endpoints(),
            DeviceAuthInfo {
                device_auth_id: "id".to_owned(),
                user_code: "code".to_owned(),
                interval_seconds: 0.0,
            },
            AbortController::new().signal(),
        )
        .await
        .expect_err("failure");
        assert_eq!(
            error.message,
            "OpenAI Codex device auth failed with status 500: {\"error\":\"server_error\",\"error_description\":\"try again later\"}"
        );
    }

    /// Pins pi `src/auth/oauth/openai-codex.ts:364-406`.
    #[tokio::test]
    async fn browser_callback_server_returns_route_validation_and_success_pages() {
        let (sender, receiver) = oneshot::channel();
        let settle = Arc::new(Mutex::new(Some(sender)));

        let not_found = browser_callback_request("/not-the-callback", settle.clone()).await;
        assert!(not_found.starts_with("HTTP/1.1 404 Not Found"));
        assert!(not_found.contains("Callback route not found."));

        let missing_state =
            browser_callback_request("/auth/callback?code=authorization-code", settle.clone())
                .await;
        assert!(missing_state.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing_state.contains("State mismatch."));

        let missing_code =
            browser_callback_request("/auth/callback?state=expected-state", settle.clone()).await;
        assert!(missing_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing_code.contains("Missing authorization code."));

        let empty_code =
            browser_callback_request("/auth/callback?state=expected-state&code=", settle.clone())
                .await;
        assert!(empty_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(empty_code.contains("Missing authorization code."));

        let success = browser_callback_request(
            "/auth/callback?state=expected-state&code=authorization-code",
            settle,
        )
        .await;
        assert!(success.starts_with("HTTP/1.1 200 OK"));
        assert!(success.contains("OpenAI authentication completed."));
        assert_eq!(
            receiver.await.expect("callback result").as_deref(),
            Some("authorization-code")
        );
    }
}
