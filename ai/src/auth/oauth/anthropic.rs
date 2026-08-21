use super::oauth_page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;
use super::{now_millis, read_loopback_request, request, send_http, write_loopback_response};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
    OAuthCredentialType, ProviderAuthInteraction,
};
use crate::types::{FetchFunction, default_fetch};
use crate::utils::abort::{AbortController, AbortReason};
use crate::utils::provider_env::get_provider_env_value;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 53_692;
const CALLBACK_PATH: &str = "/callback";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

struct AuthorizationInput {
    code: Option<String>,
    state: Option<String>,
}

type CallbackCode = Option<(String, String)>;
type CallbackSender = Arc<Mutex<Option<oneshot::Sender<CallbackCode>>>>;

struct CallbackServer {
    receiver: Option<oneshot::Receiver<CallbackCode>>,
    settle: CallbackSender,
    shutdown: watch::Sender<bool>,
}

impl CallbackServer {
    fn cancel_wait(&self) {
        if let Some(settle) = self.settle.lock().expect("callback mutex").take() {
            let _ = settle.send(None);
        }
    }

    async fn wait_for_code(&mut self) -> Option<(String, String)> {
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
        let params =
            url::form_urlencoded::parse(value.strip_prefix('?').unwrap_or(value).as_bytes())
                .collect::<Vec<_>>();
        return AuthorizationInput {
            code: params
                .iter()
                .find(|(name, _)| name == "code")
                .map(|(_, value)| value.to_string()),
            state: params
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
) -> Result<(String, String), AuthError> {
    let parsed = parse_authorization_input(input);
    if parsed
        .state
        .as_deref()
        .filter(|state| !state.is_empty())
        .is_some_and(|state| state != expected_state)
    {
        return Err(AuthError::new("OAuth state mismatch"));
    }
    let code = parsed
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| AuthError::new("Missing authorization code"))?;
    let state = parsed.state.unwrap_or_else(|| expected_state.to_owned());
    if state.is_empty() {
        return Err(AuthError::new("Missing OAuth state"));
    }
    Ok((code, state))
}

async fn handle_callback_connection(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    expected_state: &str,
    settle: &CallbackSender,
) {
    let Ok(request) = read_loopback_request(stream).await else {
        return;
    };
    let Ok(url) = url::Url::parse(&format!("http://localhost{}", request.target)) else {
        let _ = write_loopback_response(
            stream,
            500,
            "text/plain; charset=utf-8",
            None,
            "Internal error",
        )
        .await;
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
    let oauth_error = url
        .query_pairs()
        .find(|(name, _)| name == "error")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    if let Some(error) = oauth_error {
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html(
                "Anthropic authentication did not complete.",
                Some(&format!("Error: {error}")),
            ),
        )
        .await;
        return;
    }
    let code = url
        .query_pairs()
        .find(|(name, _)| name == "code")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let state = url
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let (Some(code), Some(state)) = (code, state) else {
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            None,
            &oauth_error_html("Missing code or state parameter.", None),
        )
        .await;
        return;
    };
    if state != expected_state {
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
    let _ = write_loopback_response(
        stream,
        200,
        "text/html; charset=utf-8",
        None,
        &oauth_success_html("Anthropic authentication completed. You can close this window."),
    )
    .await;
    if let Some(settle) = settle.lock().expect("callback mutex").take() {
        let _ = settle.send(Some((code, state)));
    }
}

async fn start_callback_server(
    expected_state: String,
    callback_host: String,
) -> Result<CallbackServer, AuthError> {
    let listener = TcpListener::bind((callback_host.as_str(), CALLBACK_PORT))
        .await
        .map_err(|error| AuthError::new(error.to_string()))?;
    let (settle_sender, receiver) = oneshot::channel();
    let settle = Arc::new(Mutex::new(Some(settle_sender)));
    let task_settle = settle.clone();
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                biased;
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() || *shutdown_receiver.borrow() {
                        break;
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                if let Some(settle) = task_settle.lock().expect("callback mutex").take() {
                    let _ = settle.send(None);
                }
                break;
            };
            handle_callback_connection(&mut stream, &expected_state, &task_settle).await;
        }
    });
    Ok(CallbackServer {
        receiver: Some(receiver),
        settle,
        shutdown,
    })
}

fn pending_callback_server() -> CallbackServer {
    let (settle_sender, receiver) = oneshot::channel();
    let (shutdown, _) = watch::channel(false);
    CallbackServer {
        receiver: Some(receiver),
        settle: Arc::new(Mutex::new(Some(settle_sender))),
        shutdown,
    }
}

async fn post_json(
    fetch: Arc<dyn FetchFunction>,
    url: String,
    body: Value,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<String, AuthError> {
    let response = send_http(
        fetch,
        request(
            "POST",
            url.clone(),
            [
                ("Content-Type", "application/json"),
                ("Accept", "application/json"),
            ],
            serde_json::to_vec(&body).expect("serializable OAuth body"),
            signal,
        ),
        Some(Duration::from_secs(30)),
    )
    .await
    .map_err(|error| AuthError::new(error.to_string()))?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "HTTP request failed. status={}; url={url}; body={}",
            response.status, response.body
        )));
    }
    Ok(response.body)
}

fn parse_token_body(body: &str, operation: &str) -> Result<(String, String, f64), AuthError> {
    let data = serde_json::from_str::<Value>(body).map_err(|error| {
        AuthError::new(format!(
            "{operation} returned invalid JSON. url={TOKEN_URL}; body={body}; details={error}"
        ))
    })?;
    let access = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new(format!("{operation} response missing access_token")))?;
    let refresh = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::new(format!("{operation} response missing refresh_token")))?;
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_f64)
        .ok_or_else(|| AuthError::new(format!("{operation} response missing expires_in")))?;
    Ok((access.to_owned(), refresh.to_owned(), expires_in))
}

async fn exchange_authorization_code(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    code: String,
    state: String,
    verifier: String,
    redirect_uri: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let response_body = post_json(
        fetch,
        token_url.clone(),
        json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }),
        signal,
    )
    .await
    .map_err(|error| {
        AuthError::new(format!(
            "Token exchange request failed. url={token_url}; redirect_uri={REDIRECT_URI}; response_type=authorization_code; details={error}"
        ))
    })?;
    let (access, refresh, expires_in) = parse_token_body(&response_body, "Token exchange")?;
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        refresh,
        access,
        expires: now_millis() + expires_in * 1_000.0 - 5.0 * 60.0 * 1_000.0,
        extra: Map::new(),
    })
}

async fn login_anthropic(
    fetch: Arc<dyn FetchFunction>,
    authorize_url: String,
    token_url: String,
    callback_host: String,
    listen_for_callback: bool,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate_pkce()?;
    let mut server = if listen_for_callback {
        start_callback_server(pkce.verifier.clone(), callback_host).await?
    } else {
        pending_callback_server()
    };
    let manual_abort = AbortController::new();
    let mut auth_url = url::Url::parse(&authorize_url).expect("static authorize URL");
    auth_url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", pkce.verifier.as_str()),
    ]);
    interaction.interaction.notify(AuthEvent::AuthUrl {
        url: auth_url.to_string(),
        instructions: Some("Complete login in your browser. If the browser is on another machine, paste the final redirect URL here.".to_owned()),
    });
    let prompt = interaction.interaction.prompt(AuthPrompt::ManualCode {
        message:
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                .to_owned(),
        placeholder: Some(REDIRECT_URI.to_owned()),
        signal: Some(manual_abort.signal()),
    });
    tokio::pin!(prompt);
    let result = tokio::select! {
        biased;
        callback = server.wait_for_code() => {
            match callback {
                Some((code, state)) => Ok((code, state)),
                None => parse_manual_authorization_input(&prompt.await?, &pkce.verifier),
            }
        },
        input = &mut prompt => {
            server.cancel_wait();
            parse_manual_authorization_input(&input?, &pkce.verifier)
        },
        _ = interaction.signal.cancelled() => {
            server.cancel_wait();
            parse_manual_authorization_input(&prompt.await?, &pkce.verifier)
        }
    };
    let credential = match result {
        Ok((code, state)) => {
            interaction.interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for tokens...".to_owned(),
            });
            exchange_authorization_code(
                fetch,
                token_url,
                code,
                state,
                pkce.verifier,
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

async fn refresh_anthropic_token(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    refresh_token: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let response_body = post_json(
        fetch,
        token_url.clone(),
        json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }),
        signal,
    )
    .await
    .map_err(|error| {
        AuthError::new(format!(
            "Anthropic token refresh request failed. url={token_url}; details={error}"
        ))
    })?;
    let (access, refresh, expires_in) =
        parse_token_body(&response_body, "Anthropic token refresh")?;
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        refresh,
        access,
        expires: now_millis() + expires_in * 1_000.0 - 5.0 * 60.0 * 1_000.0,
        extra: Map::new(),
    })
}

fn anthropic_oauth_with(
    fetch: Arc<dyn FetchFunction>,
    authorize_url: String,
    token_url: String,
    callback_host: Option<String>,
    listen_for_callback: bool,
) -> OAuthAuth {
    let login_fetch = fetch.clone();
    let login_token_url = token_url.clone();
    OAuthAuth {
        name: "Anthropic (Claude Pro/Max)".to_owned(),
        is_subscription: Some(true),
        login_label: None,
        login: Arc::new(move |interaction| {
            let host = callback_host.clone().unwrap_or_else(|| {
                get_provider_env_value("PI_OAUTH_CALLBACK_HOST", None)
                    .filter(|host| !host.is_empty())
                    .unwrap_or_else(|| "127.0.0.1".to_owned())
            });
            Box::pin(login_anthropic(
                login_fetch.clone(),
                authorize_url.clone(),
                login_token_url.clone(),
                host,
                listen_for_callback,
                interaction,
            )) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            Box::pin(refresh_anthropic_token(
                fetch.clone(),
                token_url.clone(),
                credential.refresh,
                signal,
            )) as AuthFuture<OAuthCredential>
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

pub fn anthropic_oauth() -> OAuthAuth {
    let callback_host = get_provider_env_value("PI_OAUTH_CALLBACK_HOST", None)
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    anthropic_oauth_with(
        default_fetch(),
        AUTHORIZE_URL.to_owned(),
        TOKEN_URL.to_owned(),
        Some(callback_host),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::AuthInteraction;
    use crate::utils::abort::{AbortController, AbortReason};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    async fn callback_request(target: &str, settle: CallbackSender) -> String {
        let (mut client, mut server) = tokio::io::duplex(16_384);
        client
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("request");
        let task = tokio::spawn(async move {
            handle_callback_connection(&mut server, "expected-state", &settle).await;
        });
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("response");
        task.await.expect("handler");
        response
    }

    struct ManualInteraction {
        auth_url: Mutex<Option<String>>,
        prompt_signal: Mutex<Option<Arc<dyn crate::types::AbortSignal>>>,
    }

    struct DeferredManualInteraction {
        signal: Arc<dyn crate::types::AbortSignal>,
        input: Mutex<Option<oneshot::Receiver<String>>>,
        prompt_started: AtomicBool,
    }

    impl AuthInteraction for ManualInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            let AuthPrompt::ManualCode { signal, .. } = prompt else {
                return Box::pin(async { Err(AuthError::new("unexpected prompt")) });
            };
            *self.prompt_signal.lock().expect("prompt signal") = signal;
            let url = self.auth_url.lock().expect("auth url").clone();
            Box::pin(async move {
                let url = url.ok_or_else(|| AuthError::new("missing auth URL"))?;
                let url =
                    url::Url::parse(&url).map_err(|error| AuthError::new(error.to_string()))?;
                let state = url
                    .query_pairs()
                    .find(|(name, _)| name == "state")
                    .map(|(_, value)| value.into_owned())
                    .expect("state");
                Ok(format!("{REDIRECT_URI}?code=manual-code&state={state}"))
            })
        }

        fn notify(&self, event: AuthEvent) {
            if let AuthEvent::AuthUrl { url, .. } = event {
                *self.auth_url.lock().expect("auth url") = Some(url);
            }
        }
    }

    impl AuthInteraction for DeferredManualInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            Some(self.signal.clone())
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            assert!(matches!(prompt, AuthPrompt::ManualCode { .. }));
            self.prompt_started.store(true, Ordering::SeqCst);
            let input = self
                .input
                .lock()
                .expect("input")
                .take()
                .expect("one prompt");
            Box::pin(async move {
                input
                    .await
                    .map_err(|_| AuthError::new("manual prompt closed"))
            })
        }

        fn notify(&self, _event: AuthEvent) {}
    }

    /// Ports pi `test/anthropic-oauth.test.ts:41` and `:110`.
    #[tokio::test]
    async fn manual_login_keeps_localhost_redirect_and_dismisses_prompt() {
        let request_body = Arc::new(Mutex::new(None::<Value>));
        let captured = request_body.clone();
        let fetcher = fetch(move |request| {
            *captured.lock().expect("body") = serde_json::from_slice(&request.body).ok();
            Ok(response(
                200,
                r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
            ))
        });
        let interaction = Arc::new(ManualInteraction {
            auth_url: Mutex::new(None),
            prompt_signal: Mutex::new(None),
        });
        let oauth = anthropic_oauth_with(
            fetcher,
            AUTHORIZE_URL.to_owned(),
            TOKEN_URL.to_owned(),
            Some("127.0.0.1".to_owned()),
            false,
        );
        let credential = (oauth.login)(crate::auth::helpers::normalize_interaction(
            interaction.clone(),
        ))
        .await
        .expect("credential");
        assert_eq!(credential.access, "access");
        let body = request_body.lock().expect("body").clone().expect("body");
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
        assert_eq!(body["code"], "manual-code");
        assert!(
            interaction
                .prompt_signal
                .lock()
                .expect("signal")
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
        );
    }

    /// Ports pi `test/anthropic-oauth.test.ts:78`.
    #[tokio::test]
    async fn refresh_omits_scope() {
        let request_body = Arc::new(Mutex::new(None::<Value>));
        let captured = request_body.clone();
        let fetcher = fetch(move |request| {
            *captured.lock().expect("body") = serde_json::from_slice(&request.body).ok();
            Ok(response(
                200,
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            ))
        });
        let oauth = anthropic_oauth_with(
            fetcher,
            AUTHORIZE_URL.to_owned(),
            TOKEN_URL.to_owned(),
            None,
            true,
        );
        let credential = OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh-token".to_owned(),
            access: "old".to_owned(),
            expires: 0.0,
            extra: Map::new(),
        };
        let refreshed = (oauth.refresh)(credential, AbortController::new().signal())
            .await
            .expect("refresh");
        assert_eq!(refreshed.access, "new-access");
        let body = request_body.lock().expect("body").clone().expect("body");
        assert_eq!(body["grant_type"], "refresh_token");
        assert!(body.get("scope").is_none());
    }

    /// Pins pi `src/auth/oauth/anthropic.ts:52-80` and `:285-304`.
    #[test]
    fn manual_input_matches_url_search_params_and_javascript_falsiness() {
        assert_eq!(
            parse_manual_authorization_input("?code=authorization-code", "expected-state")
                .expect("leading question mark"),
            ("authorization-code".to_owned(), "expected-state".to_owned())
        );
        assert_eq!(
            parse_manual_authorization_input("authorization-code#", "expected-state")
                .expect_err("empty state")
                .message,
            "Missing OAuth state"
        );
        assert_eq!(
            parse_manual_authorization_input(
                "http://localhost/callback?code=authorization-code&state=",
                "expected-state",
            )
            .expect_err("empty URL state")
            .message,
            "Missing OAuth state"
        );
    }

    /// Pins pi `src/auth/oauth/anthropic.ts:238-306`: interaction abort cancels only the callback wait.
    #[tokio::test]
    async fn interaction_abort_still_waits_for_the_manual_prompt() {
        let controller = AbortController::new();
        let (send_input, receive_input) = oneshot::channel();
        let interaction = Arc::new(DeferredManualInteraction {
            signal: controller.signal(),
            input: Mutex::new(Some(receive_input)),
            prompt_started: AtomicBool::new(false),
        });
        let login = login_anthropic(
            fetch(|_| {
                Ok(response(
                    200,
                    r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
                ))
            }),
            AUTHORIZE_URL.to_owned(),
            TOKEN_URL.to_owned(),
            "127.0.0.1".to_owned(),
            false,
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        );
        let mut task = tokio::spawn(login);
        while !interaction.prompt_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        controller.abort(AbortReason::default_abort());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut task)
                .await
                .is_err()
        );
        assert!(send_input.send("manual-code".to_owned()).is_ok());
        let error = task.await.expect("task").expect_err("aborted exchange");
        assert!(error.message.contains("request aborted"));
    }

    /// Pins pi `src/auth/oauth/anthropic.ts:103-153`.
    #[tokio::test]
    async fn callback_server_returns_route_validation_and_success_pages() {
        let (sender, receiver) = oneshot::channel();
        let settle = Arc::new(Mutex::new(Some(sender)));

        let not_found = callback_request("/not-the-callback", settle.clone()).await;
        assert!(not_found.starts_with("HTTP/1.1 404 Not Found"));
        assert!(not_found.contains("Callback route not found."));

        let missing = callback_request("/callback?code=only-code", settle.clone()).await;
        assert!(missing.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing.contains("Missing code or state parameter."));

        let empty = callback_request("/callback?code=&state=", settle.clone()).await;
        assert!(empty.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(empty.contains("Missing code or state parameter."));

        let mismatch = callback_request("/callback?code=code&state=wrong", settle.clone()).await;
        assert!(mismatch.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(mismatch.contains("State mismatch."));

        let success = callback_request(
            "/callback?code=authorization-code&state=expected-state",
            settle,
        )
        .await;
        assert!(success.starts_with("HTTP/1.1 200 OK"));
        assert!(success.contains("Anthropic authentication completed."));
        assert_eq!(
            receiver.await.expect("callback result"),
            Some(("authorization-code".to_owned(), "expected-state".to_owned()))
        );
    }
}
