use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::oauth_page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;
use super::{
    OAuthHttpError, now_millis, random_uuid_v4, read_loopback_request, request, send_http,
    write_loopback_response,
};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, AuthPrompt, AuthSelectOption, ModelAuth, OAuthAuth,
    OAuthCredential, OAuthCredentialType, ProviderAuthInteraction,
};
use crate::providers::radius_config::normalize_radius_gateway_url;
use crate::types::{FetchFunction, default_fetch};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 1_456;
const CALLBACK_PATH: &str = "/oauth/callback";
const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: f64 = 60_000.0;
const LOGIN_METHOD_BROWSER: &str = "browser";
const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";
const OAUTH_CLIENT_ID: &str = "pi-gateway";
const OAUTH_SCOPE: &str = "gateway offline_access";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

struct RadiusOAuthDiscovery {
    authorization_endpoint: String,
}

struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: f64,
    interval: Option<f64>,
}

#[derive(Debug)]
struct OAuthResponseError {
    oauth_error: Option<String>,
    message: String,
}

impl fmt::Display for OAuthResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

enum RadiusRequestError {
    Auth(AuthError),
    OAuth(OAuthResponseError),
}

impl From<RadiusRequestError> for AuthError {
    fn from(error: RadiusRequestError) -> Self {
        match error {
            RadiusRequestError::Auth(error) => error,
            RadiusRequestError::OAuth(error) => AuthError::new(error.message),
        }
    }
}

type OAuthCallbackSender = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

struct OAuthCallbackServer {
    receiver: Option<oneshot::Receiver<Option<String>>>,
    settle: OAuthCallbackSender,
    shutdown: watch::Sender<bool>,
}

impl OAuthCallbackServer {
    async fn wait_for_code(&mut self) -> Option<String> {
        self.receiver
            .take()
            .expect("callback wait called once")
            .await
            .unwrap_or(None)
    }

    fn finish(&self, code: Option<String>) {
        if let Some(settle) = self.settle.lock().expect("callback mutex").take() {
            let _ = settle.send(code);
        }
    }

    fn close(&self) {
        self.finish(None);
        self.shutdown.send_replace(true);
    }
}

fn gateway_url(gateway: &str, path: &str) -> Result<String, AuthError> {
    url::Url::parse(gateway)
        .and_then(|gateway| gateway.join(path))
        .map(|url| url.to_string())
        .map_err(|error| AuthError::new(error.to_string()))
}

async fn load_radius_oauth_discovery(
    fetch: Arc<dyn FetchFunction>,
    gateway: &str,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<RadiusOAuthDiscovery, AuthError> {
    let response = send_http(
        fetch,
        request(
            "GET",
            gateway_url(gateway, "/v1/oauth")?,
            [("accept", "application/json")],
            Vec::new(),
            signal,
        ),
        None,
    )
    .await
    .map_err(|error| AuthError::new(error.to_string()))?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "Could not load Radius OAuth config from {gateway}: {} {}",
            response.status, response.body
        )));
    }
    let data = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| AuthError::new(error.to_string()))?;
    let authorization_endpoint = data
        .get("authorizationEndpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new(format!("Invalid Radius OAuth config from {gateway}")))?;
    Ok(RadiusOAuthDiscovery {
        authorization_endpoint: authorization_endpoint.to_owned(),
    })
}

fn read_oauth_response_error(status: u16, text: &str, message: &str) -> OAuthResponseError {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let oauth_error = parsed
        .as_ref()
        .and_then(|data| data.get("error"))
        .and_then(Value::as_str)
        .filter(|oauth_error| !oauth_error.is_empty())
        .map(str::to_owned);
    let description = parsed
        .as_ref()
        .and_then(|data| data.get("error_description"))
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
        .map(str::to_owned)
        .or_else(|| (!text.is_empty() && parsed.is_none()).then(|| text.to_owned()));
    let detail = match (oauth_error.as_ref(), description.as_ref()) {
        (Some(oauth_error), Some(description)) => format!("{oauth_error}: {description}"),
        (Some(oauth_error), None) => oauth_error.clone(),
        (None, Some(description)) => description.clone(),
        (None, None) => status.to_string(),
    };
    OAuthResponseError {
        oauth_error,
        message: format!("{message}: {detail}"),
    }
}

async fn request_oauth_token(
    fetch: Arc<dyn FetchFunction>,
    gateway: &str,
    body: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, RadiusRequestError> {
    let url = gateway_url(gateway, "/v1/oauth/token").map_err(RadiusRequestError::Auth)?;
    let response = send_http(
        fetch,
        request(
            "POST",
            url,
            [
                ("accept", "application/json"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            body,
            signal.clone(),
        ),
        None,
    )
    .await
    .map_err(|error| {
        RadiusRequestError::Auth(match error {
            OAuthHttpError::Aborted => AuthError::new("Login cancelled"),
            other => AuthError::new(other.to_string()),
        })
    })?;
    if !response.ok() {
        return Err(RadiusRequestError::OAuth(read_oauth_response_error(
            response.status,
            &response.body,
            "Radius OAuth token request failed",
        )));
    }
    let data = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| RadiusRequestError::Auth(AuthError::new(error.to_string())))?;
    let access = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RadiusRequestError::Auth(AuthError::new(
                "Radius OAuth token response is missing access_token",
            ))
        })?;
    let refresh = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RadiusRequestError::Auth(AuthError::new(
                "Radius OAuth token response is missing refresh_token",
            ))
        })?;
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            RadiusRequestError::Auth(AuthError::new(
                "Radius OAuth token response is missing expires_in",
            ))
        })?;
    let mut extra = Map::new();
    if let Some(scope) = data.get("scope").and_then(Value::as_str) {
        extra.insert("scope".to_owned(), Value::String(scope.to_owned()));
    }
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now_millis() + expires_in * 1_000.0 - TOKEN_EXPIRY_SKEW_MS,
        extra,
    })
}

async fn handle_oauth_callback_connection(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    expected_state: &str,
    settle: &OAuthCallbackSender,
) {
    let Ok(request) = read_loopback_request(stream).await else {
        return;
    };
    let Ok(url) = url::Url::parse(&format!("http://{CALLBACK_HOST}{}", request.target)) else {
        return;
    };
    if url.path() != CALLBACK_PATH {
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
    let state = url
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned());
    if state.as_deref() != Some(expected_state) {
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("OAuth state mismatch.", None),
        )
        .await;
        return;
    }
    if let Some(error) = url
        .query_pairs()
        .find(|(name, _)| name == "error")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
    {
        let description = url
            .query_pairs()
            .find(|(name, _)| name == "error_description")
            .map(|(_, value)| value.into_owned())
            .unwrap_or(error);
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html(&description, None),
        )
        .await;
        if let Some(settle) = settle.lock().expect("callback mutex").take() {
            let _ = settle.send(None);
        }
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
        &oauth_success_html("Signed in to Radius. You may now close this page."),
    )
    .await;
    if let Some(settle) = settle.lock().expect("callback mutex").take() {
        let _ = settle.send(Some(code));
    }
}

async fn start_oauth_callback_server(
    expected_state: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> OAuthCallbackServer {
    let (sender, receiver) = oneshot::channel();
    let settle = Arc::new(Mutex::new(Some(sender)));
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let listener = match TcpListener::bind((CALLBACK_HOST, CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(_) => {
            if let Some(settle) = settle.lock().expect("callback mutex").take() {
                let _ = settle.send(None);
            }
            return OAuthCallbackServer {
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
                _ = signal.cancelled() => {
                    if let Some(settle) = task_settle.lock().expect("callback mutex").take() {
                        let _ = settle.send(None);
                    }
                    break;
                }
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() || *shutdown_receiver.borrow() { break; }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                break;
            };
            handle_oauth_callback_connection(&mut stream, &expected_state, &task_settle).await;
        }
    });
    OAuthCallbackServer {
        receiver: Some(receiver),
        settle,
        shutdown,
    }
}

fn radius_authorization_url(
    authorization_endpoint: &str,
    challenge: &str,
    state: &str,
) -> Result<String, AuthError> {
    let mut authorize_url = url::Url::parse(authorization_endpoint)
        .map_err(|error| AuthError::new(error.to_string()))?;
    authorize_url.set_query(None);
    authorize_url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", OAUTH_CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", OAUTH_SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("handoff", "url"),
        ("state", state),
    ]);
    Ok(authorize_url.to_string())
}

async fn login_with_browser(
    fetch: Arc<dyn FetchFunction>,
    gateway: String,
    authorization_endpoint: String,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate_pkce()?;
    let state = random_uuid_v4().map_err(AuthError::new)?;
    let authorize_url = radius_authorization_url(&authorization_endpoint, &pkce.challenge, &state)?;
    let mut callback = start_oauth_callback_server(state, interaction.signal.clone()).await;
    interaction.interaction.notify(AuthEvent::Progress {
        message: format!("Listening for OAuth callback on {REDIRECT_URI}"),
    });
    interaction.interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some("Continue in your browser.".to_owned()),
    });
    let code = callback.wait_for_code().await;
    callback.close();
    let Some(code) = code else {
        return Err(AuthError::new(if interaction.signal.is_aborted() {
            "Login cancelled"
        } else {
            "OAuth callback did not complete."
        }));
    };
    request_oauth_token(
        fetch,
        &gateway,
        super::form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OAUTH_CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("code", &code),
            ("code_verifier", &pkce.verifier),
        ]),
        interaction.signal,
    )
    .await
    .map_err(Into::into)
}

async fn request_device_authorization(
    fetch: Arc<dyn FetchFunction>,
    gateway: &str,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<DeviceAuthorizationResponse, AuthError> {
    let response = send_http(
        fetch,
        request(
            "POST",
            gateway_url(gateway, "/v1/oauth/device")?,
            [
                ("accept", "application/json"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            super::form(&[("client_id", OAUTH_CLIENT_ID), ("scope", OAUTH_SCOPE)]),
            signal,
        ),
        None,
    )
    .await
    .map_err(|error| match error {
        OAuthHttpError::Aborted => AuthError::new("Login cancelled"),
        other => AuthError::new(other.to_string()),
    })?;
    if !response.ok() {
        return Err(AuthError::new(
            read_oauth_response_error(
                response.status,
                &response.body,
                "Radius OAuth device authorization failed",
            )
            .message,
        ));
    }
    let data = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| AuthError::new(error.to_string()))?;
    let device_code = data
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let user_code = data
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let verification_uri = data
        .get("verification_uri")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0);
    let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) =
        (device_code, user_code, verification_uri, expires_in)
    else {
        return Err(AuthError::new(
            "Radius OAuth device authorization response is missing required fields",
        ));
    };
    Ok(DeviceAuthorizationResponse {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_uri: verification_uri.to_owned(),
        expires_in,
        interval: data.get("interval").and_then(Value::as_f64),
    })
}

async fn login_with_device_code(
    fetch: Arc<dyn FetchFunction>,
    gateway: String,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let device =
        request_device_authorization(fetch.clone(), &gateway, interaction.signal.clone()).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: device.interval,
        expires_in_seconds: Some(device.expires_in),
    });
    let signal = interaction.signal.clone();
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: false,
            signal: interaction.signal,
        },
        move || {
            let fetch = fetch.clone();
            let gateway = gateway.clone();
            let device_code = device.device_code.clone();
            let signal = signal.clone();
            async move {
                match request_oauth_token(
                    fetch,
                    &gateway,
                    super::form(&[
                        ("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE),
                        ("client_id", OAUTH_CLIENT_ID),
                        ("device_code", &device_code),
                    ]),
                    signal,
                )
                .await
                {
                    Ok(credential) => Ok(OAuthDeviceCodePollResult::Complete(credential)),
                    Err(RadiusRequestError::OAuth(error)) => {
                        Ok(match error.oauth_error.as_deref() {
                            Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                            Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                                interval_seconds: None,
                            },
                            Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                                message: "Device authorization expired.".to_owned(),
                            },
                            Some("access_denied") => OAuthDeviceCodePollResult::Failed {
                                message: "Device authorization was denied.".to_owned(),
                            },
                            _ => return Err(AuthError::new(error.message)),
                        })
                    }
                    Err(RadiusRequestError::Auth(error)) => Err(error),
                }
            }
        },
    )
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusOAuthOptions {
    pub name: String,
    pub gateway: String,
}

fn create_radius_oauth_with(
    options: RadiusOAuthOptions,
    fetch: Arc<dyn FetchFunction>,
) -> OAuthAuth {
    let gateway = normalize_radius_gateway_url(&options.gateway);
    let name = options.name;
    let login_name = name.clone();
    let login_gateway = gateway.clone();
    let login_fetch = fetch.clone();
    OAuthAuth {
        name,
        is_subscription: None,
        login_label: None,
        login: Arc::new(move |interaction| {
            let fetch = login_fetch.clone();
            let gateway = login_gateway.clone();
            let name = login_name.clone();
            Box::pin(async move {
                let login_method = interaction
                    .interaction
                    .prompt(AuthPrompt::Select {
                        message: format!("Sign in to {name}:"),
                        options: vec![
                            AuthSelectOption {
                                id: LOGIN_METHOD_BROWSER.to_owned(),
                                label: "Sign in with browser (recommended)".to_owned(),
                                description: None,
                            },
                            AuthSelectOption {
                                id: LOGIN_METHOD_DEVICE_CODE.to_owned(),
                                label:
                                    "Sign in with device code (when signing in from another device)"
                                        .to_owned(),
                                description: None,
                            },
                        ],
                        signal: None,
                    })
                    .await?;
                match login_method.as_str() {
                    LOGIN_METHOD_DEVICE_CODE => {
                        login_with_device_code(fetch, gateway, interaction).await
                    }
                    LOGIN_METHOD_BROWSER => {
                        let discovery = load_radius_oauth_discovery(
                            fetch.clone(),
                            &gateway,
                            interaction.signal.clone(),
                        )
                        .await?;
                        login_with_browser(
                            fetch,
                            gateway,
                            discovery.authorization_endpoint,
                            interaction,
                        )
                        .await
                    }
                    _ => Err(AuthError::new(format!(
                        "Unknown {name} sign-in method: {login_method}"
                    ))),
                }
            }) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            let fetch = fetch.clone();
            let gateway = gateway.clone();
            Box::pin(async move {
                request_oauth_token(
                    fetch,
                    &gateway,
                    super::form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", OAUTH_CLIENT_ID),
                        ("refresh_token", &credential.refresh),
                    ]),
                    signal,
                )
                .await
                .map_err(Into::into)
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

pub fn create_radius_oauth(options: RadiusOAuthOptions) -> OAuthAuth {
    create_radius_oauth_with(options, default_fetch())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::AuthInteraction;
    use crate::utils::abort::AbortController;
    use std::collections::VecDeque;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn oauth_callback_request(target: &str, settle: OAuthCallbackSender) -> String {
        let (mut client, mut server) = tokio::io::duplex(16_384);
        client
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("request");
        let task = tokio::spawn(async move {
            handle_oauth_callback_connection(&mut server, "expected-state", &settle).await;
        });
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("response");
        task.await.expect("handler");
        response
    }

    struct Interaction {
        method: String,
        events: Mutex<Vec<AuthEvent>>,
    }

    impl AuthInteraction for Interaction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            assert!(matches!(prompt, AuthPrompt::Select { .. }));
            let method = self.method.clone();
            Box::pin(async move { Ok(method) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events").push(event);
        }
    }

    fn options() -> RadiusOAuthOptions {
        RadiusOAuthOptions {
            name: "Radius".to_owned(),
            gateway: "https://radius.example".to_owned(),
        }
    }

    fn credential() -> OAuthCredential {
        OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            access: "old-access".to_owned(),
            refresh: "old-refresh".to_owned(),
            expires: 0.0,
            extra: Map::new(),
        }
    }

    /// Ports pi `test/radius-oauth.test.ts:36`.
    #[tokio::test]
    async fn device_login_uses_gateway_endpoints_directly() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                r#"{"device_code":"device-code","user_code":"ABCD-1234","verification_uri":"https://radius-ui.example/pair","expires_in":600,"interval":5}"#,
            ),
            response(
                200,
                r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600,"scope":"gateway offline_access"}"#,
            ),
        ])));
        let urls = Arc::new(Mutex::new(Vec::new()));
        let fetcher = {
            let replies = replies.clone();
            let urls = urls.clone();
            fetch(move |request| {
                urls.lock().expect("urls").push(request.url);
                Ok(replies.lock().expect("replies").pop_front().expect("reply"))
            })
        };
        let interaction = Arc::new(Interaction {
            method: LOGIN_METHOD_DEVICE_CODE.to_owned(),
            events: Mutex::new(Vec::new()),
        });
        let credential = (create_radius_oauth_with(options(), fetcher).login)(
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        )
        .await
        .expect("credential");
        assert_eq!(credential.access, "access-token");
        assert_eq!(credential.extra["scope"], OAUTH_SCOPE);
        assert_eq!(
            urls.lock().expect("urls").as_slice(),
            [
                "https://radius.example/v1/oauth/device",
                "https://radius.example/v1/oauth/token"
            ]
        );
        assert!(
            matches!(interaction.events.lock().expect("events").as_slice(), [AuthEvent::DeviceCode { user_code, .. }] if user_code == "ABCD-1234")
        );
    }

    /// Ports pi `test/radius-oauth.test.ts:93`.
    #[tokio::test]
    async fn refresh_uses_gateway_without_discovery() {
        let seen = Arc::new(Mutex::new(None));
        let captured = seen.clone();
        let oauth = create_radius_oauth_with(
            options(),
            fetch(move |request| {
                *captured.lock().expect("request") = Some(request);
                Ok(response(
                    200,
                    r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
                ))
            }),
        );
        let refreshed = (oauth.refresh)(credential(), AbortController::new().signal())
            .await
            .expect("refresh");
        assert_eq!(refreshed.access, "new-access");
        let request = seen.lock().expect("request").take().expect("request");
        assert_eq!(request.url, "https://radius.example/v1/oauth/token");
        assert!(
            String::from_utf8(request.body)
                .expect("form")
                .contains("grant_type=refresh_token")
        );
    }

    /// Ports pi `test/radius-oauth.test.ts:118`.
    #[tokio::test]
    async fn browser_discovery_requires_only_authorization_endpoint() {
        let oauth = create_radius_oauth_with(
            options(),
            fetch(|_| Ok(response(200, r#"{"issuer":"https://radius-ui.example"}"#))),
        );
        let error = (oauth.login)(crate::auth::helpers::normalize_interaction(Arc::new(
            Interaction {
                method: LOGIN_METHOD_BROWSER.to_owned(),
                events: Mutex::new(Vec::new()),
            },
        )))
        .await
        .expect_err("invalid discovery");
        assert_eq!(
            error.message,
            "Invalid Radius OAuth config from https://radius.example"
        );
    }

    /// Pins pi `src/auth/oauth/radius.ts:73-78`'s falsy-description fallback.
    #[test]
    fn empty_oauth_error_description_falls_back_to_status() {
        let error = read_oauth_response_error(
            401,
            r#"{"error_description":""}"#,
            "Radius OAuth token request failed",
        );
        assert_eq!(error.message, "Radius OAuth token request failed: 401");
        let description = read_oauth_response_error(
            401,
            r#"{"error":"","error_description":"description"}"#,
            "Radius OAuth token request failed",
        );
        assert_eq!(
            description.message,
            "Radius OAuth token request failed: description"
        );
    }

    /// Pins pi `src/auth/oauth/radius.ts:227-237`'s `URL.search` replacement.
    #[test]
    fn browser_authorization_replaces_discovered_endpoint_query() {
        let authorize = radius_authorization_url(
            "https://radius-ui.example/authorize?audience=radius&state=stale",
            "challenge",
            "fresh-state",
        )
        .expect("authorization URL");
        let authorize = url::Url::parse(&authorize).expect("authorization URL");
        let pairs = authorize.query_pairs().collect::<Vec<_>>();
        assert!(!pairs.iter().any(|(name, _)| name == "audience"));
        assert_eq!(
            pairs
                .iter()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.as_ref()),
            Some("fresh-state")
        );
        assert_eq!(pairs.len(), 8);
    }

    /// Pins pi `src/auth/oauth/radius.ts:162-210`.
    #[tokio::test]
    async fn browser_callback_server_returns_route_validation_and_success_pages() {
        let (sender, receiver) = oneshot::channel();
        let settle = Arc::new(Mutex::new(Some(sender)));

        let not_found = oauth_callback_request("/not-the-callback", settle.clone()).await;
        assert!(not_found.starts_with("HTTP/1.1 404 Not Found"));
        assert!(not_found.contains("Callback route not found."));

        let missing_state =
            oauth_callback_request("/oauth/callback?code=authorization-code", settle.clone()).await;
        assert!(missing_state.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing_state.contains("OAuth state mismatch."));

        let missing_code =
            oauth_callback_request("/oauth/callback?state=expected-state", settle.clone()).await;
        assert!(missing_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing_code.contains("Missing authorization code."));

        let empty_code =
            oauth_callback_request("/oauth/callback?state=expected-state&code=", settle.clone())
                .await;
        assert!(empty_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(empty_code.contains("Missing authorization code."));

        let success = oauth_callback_request(
            "/oauth/callback?state=expected-state&error=&code=authorization-code",
            settle,
        )
        .await;
        assert!(success.starts_with("HTTP/1.1 200 OK"));
        assert!(success.contains("Signed in to Radius."));
        assert_eq!(
            receiver.await.expect("callback result").as_deref(),
            Some("authorization-code")
        );
    }
}
