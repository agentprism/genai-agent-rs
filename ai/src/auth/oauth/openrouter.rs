use super::oauth_page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;
use super::{
    OAuthHttpError, random_uuid_v4, read_loopback_request, request, send_http,
    write_loopback_response,
};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
    OAuthCredentialType, ProviderAuthInteraction,
};
use crate::types::{FetchFunction, default_fetch};
use crate::utils::abort::{AbortController, AbortReason};
use crate::utils::provider_env::get_provider_env_value;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

type CallbackResult = Result<Option<OAuthCredential>, AuthError>;

struct CallbackServer {
    callback_url: String,
    receiver: Option<oneshot::Receiver<CallbackResult>>,
    settle: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
    claimed: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
}

impl CallbackServer {
    fn close(&self) {
        self.shutdown.send_replace(true);
    }

    fn cancel_wait(&self) {
        if !self.claimed.load(Ordering::SeqCst)
            && let Some(settle) = self.settle.lock().expect("callback mutex").take()
        {
            let _ = settle.send(Ok(None));
            self.close();
        }
    }

    async fn wait_for_credential(&mut self) -> CallbackResult {
        self.receiver
            .take()
            .expect("callback wait called once")
            .await
            .unwrap_or_else(|_| Err(AuthError::new("OpenRouter OAuth callback closed")))
    }
}

fn get_callback_host() -> String {
    get_provider_env_value("PI_OAUTH_CALLBACK_HOST", None)
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(value) {
        return url
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty());
    }
    if value.contains("code=") {
        return url::form_urlencoded::parse(value.strip_prefix('?').unwrap_or(value).as_bytes())
            .find(|(name, _)| name == "code")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty());
    }
    Some(value.to_owned())
}

fn error_detail(body: &Map<String, Value>) -> Option<String> {
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
        .map(str::to_owned)
}

async fn exchange_authorization_code(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    code: String,
    verifier: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    if signal.is_aborted() {
        return Err(AuthError::new("Login cancelled"));
    }
    let response = send_http(
        fetch,
        request(
            "POST",
            token_url,
            [
                ("accept", "application/json"),
                ("content-type", "application/json"),
            ],
            serde_json::to_vec(&json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": "S256",
            }))
            .expect("serializable JSON"),
            signal.clone(),
        ),
        Some(TOKEN_EXCHANGE_TIMEOUT),
    )
    .await
    .map_err(|error| match error {
        OAuthHttpError::Aborted if signal.is_aborted() => AuthError::new("Login cancelled"),
        OAuthHttpError::Timeout => AuthError::new("OpenRouter OAuth token exchange timed out"),
        other => AuthError::new(other.to_string()),
    })?;
    let body = match serde_json::from_str::<Value>(&response.body) {
        Ok(value) => value.as_object().cloned().unwrap_or_default(),
        Err(_) if response.ok() => {
            return Err(AuthError::new("OpenRouter OAuth returned invalid JSON"));
        }
        Err(_) => Map::new(),
    };
    if !response.ok() {
        let detail = error_detail(&body)
            .filter(|detail| !detail.is_empty())
            .map(|detail| format!(": {detail}"))
            .unwrap_or_default();
        return Err(AuthError::new(format!(
            "OpenRouter OAuth key exchange failed (HTTP {}){detail}",
            response.status
        )));
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| AuthError::new("OpenRouter OAuth response carries no \"key\""))?;
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        access: key.to_owned(),
        refresh: String::new(),
        expires: JS_MAX_SAFE_INTEGER,
        extra: Map::new(),
    })
}

fn finish_callback(
    settle: &Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
    shutdown: &watch::Sender<bool>,
    result: CallbackResult,
) {
    if let Some(settle) = settle.lock().expect("callback mutex").take() {
        let _ = settle.send(result);
        shutdown.send_replace(true);
    }
}

#[derive(Clone)]
struct CallbackHandler {
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    callback_path: String,
    verifier: String,
    signal: Arc<dyn crate::types::AbortSignal>,
    callback_host: String,
    settle: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
    claimed: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
}

async fn handle_callback_connection(
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    handler: CallbackHandler,
) {
    let Ok(request) = read_loopback_request(stream).await else {
        return;
    };
    let Ok(url) = url::Url::parse(&format!(
        "http://{}{}",
        handler.callback_host, request.target
    )) else {
        return;
    };
    if request.method != "GET" || url.path() != handler.callback_path {
        let _ = write_loopback_response(
            stream,
            404,
            "text/html; charset=utf-8",
            Some("no-store"),
            &oauth_error_html("OAuth callback route not found.", None),
        )
        .await;
        return;
    }
    if handler.claimed.load(Ordering::SeqCst)
        || handler.settle.lock().expect("callback mutex").is_none()
    {
        let _ = write_loopback_response(
            stream,
            409,
            "text/html; charset=utf-8",
            Some("no-store"),
            &oauth_error_html("This OAuth callback has already been used.", None),
        )
        .await;
        return;
    }
    if let Some(oauth_error) = url
        .query_pairs()
        .find(|(name, _)| name == "error")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
    {
        let description = url
            .query_pairs()
            .find(|(name, _)| name == "error_description")
            .map(|(_, value)| value.into_owned())
            .unwrap_or(oauth_error);
        let _ = write_loopback_response(
            stream,
            400,
            "text/html; charset=utf-8",
            Some("no-store"),
            &oauth_error_html("OpenRouter authorization was denied.", Some(&description)),
        )
        .await;
        finish_callback(
            &handler.settle,
            &handler.shutdown,
            Err(AuthError::new(format!(
                "OpenRouter authorization failed: {description}"
            ))),
        );
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
            Some("no-store"),
            &oauth_error_html("OpenRouter returned no authorization code.", None),
        )
        .await;
        return;
    };
    if handler.claimed.swap(true, Ordering::SeqCst) {
        let _ = write_loopback_response(
            stream,
            409,
            "text/html; charset=utf-8",
            Some("no-store"),
            &oauth_error_html("This OAuth callback has already been used.", None),
        )
        .await;
        return;
    }
    match exchange_authorization_code(
        handler.fetch,
        handler.token_url,
        code,
        handler.verifier,
        handler.signal,
    )
    .await
    {
        Ok(credential) => {
            let _ = write_loopback_response(
                stream,
                200,
                "text/html; charset=utf-8",
                Some("no-store"),
                &oauth_success_html("Signed in to OpenRouter. You may now close this page."),
            )
            .await;
            finish_callback(&handler.settle, &handler.shutdown, Ok(Some(credential)));
        }
        Err(error) => {
            let _ = write_loopback_response(
                stream,
                502,
                "text/html; charset=utf-8",
                Some("no-store"),
                &oauth_error_html("OpenRouter key exchange failed.", Some(&error.message)),
            )
            .await;
            finish_callback(&handler.settle, &handler.shutdown, Err(error));
        }
    }
}

async fn start_callback_server(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    callback_path: String,
    verifier: String,
    signal: Arc<dyn crate::types::AbortSignal>,
    callback_host: String,
) -> Result<CallbackServer, AuthError> {
    if signal.is_aborted() {
        return Err(AuthError::new("Login cancelled"));
    }
    let listener = TcpListener::bind((callback_host.as_str(), 0))
        .await
        .map_err(|error| AuthError::new(error.to_string()))?;
    if signal.is_aborted() {
        return Err(AuthError::new("Login cancelled"));
    }
    let port = listener
        .local_addr()
        .map_err(|_| AuthError::new("Could not determine the OpenRouter OAuth callback port"))?
        .port();
    let callback_url = format!("http://{callback_host}:{port}{callback_path}");
    let (sender, receiver) = oneshot::channel();
    let settle = Arc::new(Mutex::new(Some(sender)));
    let claimed = Arc::new(AtomicBool::new(false));
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let task_settle = settle.clone();
    let task_claimed = claimed.clone();
    let task_shutdown = shutdown.clone();
    let handler = CallbackHandler {
        fetch,
        token_url,
        callback_path,
        verifier,
        signal: signal.clone(),
        callback_host,
        settle: task_settle.clone(),
        claimed: task_claimed,
        shutdown: task_shutdown.clone(),
    };
    tokio::spawn(async move {
        let timeout = tokio::time::sleep(LOGIN_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            let accepted = tokio::select! {
                biased;
                _ = signal.cancelled() => {
                    finish_callback(&task_settle, &task_shutdown, Err(AuthError::new("Login cancelled")));
                    break;
                }
                _ = &mut timeout => {
                    finish_callback(&task_settle, &task_shutdown, Err(AuthError::new("OpenRouter OAuth login timed out")));
                    break;
                }
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() || *shutdown_receiver.borrow() { break; }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                finish_callback(
                    &task_settle,
                    &task_shutdown,
                    Err(AuthError::new("OpenRouter OAuth callback server failed")),
                );
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                handle_callback_connection(&mut stream, handler).await;
            });
        }
    });
    Ok(CallbackServer {
        callback_url,
        receiver: Some(receiver),
        settle,
        claimed,
        shutdown,
    })
}

fn pending_callback_server(callback_url: String) -> CallbackServer {
    let (sender, receiver) = oneshot::channel();
    let (shutdown, _) = watch::channel(false);
    CallbackServer {
        callback_url,
        receiver: Some(receiver),
        settle: Arc::new(Mutex::new(Some(sender))),
        claimed: Arc::new(AtomicBool::new(false)),
        shutdown,
    }
}

async fn login_openrouter(
    fetch: Arc<dyn FetchFunction>,
    authorize_url: String,
    token_url: String,
    callback_host: Option<String>,
    listen_for_callback: bool,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let pkce = generate_pkce()?;
    let callback_path = format!(
        "/oauth/callback/{}",
        random_uuid_v4().map_err(AuthError::new)?
    );
    let callback_host = callback_host.unwrap_or_else(get_callback_host);
    let mut callback = if listen_for_callback {
        start_callback_server(
            fetch.clone(),
            token_url.clone(),
            callback_path,
            pkce.verifier.clone(),
            interaction.signal.clone(),
            callback_host,
        )
        .await?
    } else {
        pending_callback_server(format!("http://{callback_host}:1{callback_path}"))
    };
    let manual_abort = AbortController::new();
    let mut authorize =
        url::Url::parse(&authorize_url).map_err(|error| AuthError::new(error.to_string()))?;
    authorize.query_pairs_mut().extend_pairs([
        ("callback_url", callback.callback_url.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    interaction.interaction.notify(AuthEvent::Progress {
        message: format!(
            "Listening for OpenRouter OAuth callback on {}",
            callback.callback_url
        ),
    });
    interaction.interaction.notify(AuthEvent::AuthUrl {
        url: authorize.to_string(),
        instructions: Some("Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".to_owned()),
    });
    let prompt = interaction.interaction.prompt(AuthPrompt::ManualCode {
        message:
            "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                .to_owned(),
        placeholder: Some(callback.callback_url.clone()),
        signal: Some(manual_abort.signal()),
    });
    tokio::pin!(prompt);
    let result = tokio::select! {
        biased;
        _ = interaction.signal.cancelled() => Err(AuthError::new("Login cancelled")),
        credential = callback.wait_for_credential() => {
            match credential? {
                Some(credential) => Ok(credential),
                None => {
                    let code = parse_authorization_input(&prompt.await?)
                        .ok_or_else(|| AuthError::new("Missing authorization code"))?;
                    interaction.interaction.notify(AuthEvent::Progress {
                        message: "Exchanging authorization code for an API key...".to_owned(),
                    });
                    exchange_authorization_code(fetch, token_url, code, pkce.verifier, interaction.signal).await
                }
            }
        }
        input = &mut prompt => {
            callback.cancel_wait();
            let code = parse_authorization_input(&input?)
                .ok_or_else(|| AuthError::new("Missing authorization code"))?;
            interaction.interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for an API key...".to_owned(),
            });
            exchange_authorization_code(fetch, token_url, code, pkce.verifier, interaction.signal).await
        }
    };
    manual_abort.abort(AbortReason::default_abort());
    callback.close();
    result
}

fn openrouter_oauth_with(
    fetch: Arc<dyn FetchFunction>,
    authorize_url: String,
    token_url: String,
    callback_host: Option<String>,
    listen_for_callback: bool,
) -> OAuthAuth {
    OAuthAuth {
        name: "OpenRouter OAuth".to_owned(),
        is_subscription: None,
        login_label: Some("Sign in with OpenRouter".to_owned()),
        login: Arc::new(move |interaction| {
            Box::pin(login_openrouter(
                fetch.clone(),
                authorize_url.clone(),
                token_url.clone(),
                callback_host.clone(),
                listen_for_callback,
                interaction,
            )) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(|credential, _| Box::pin(async move { Ok(credential) })),
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

pub fn openrouter_oauth() -> OAuthAuth {
    openrouter_oauth_with(
        default_fetch(),
        AUTHORIZE_URL.to_owned(),
        TOKEN_URL.to_owned(),
        None,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::AuthInteraction;
    use crate::types::{ProviderHttpRequest, ProviderHttpResponse};
    use crate::utils::abort::AbortController;
    use futures::future::BoxFuture;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    fn callback_handler(
        callback: &CallbackServer,
        fetch: Arc<dyn FetchFunction>,
    ) -> CallbackHandler {
        CallbackHandler {
            fetch,
            token_url: TOKEN_URL.to_owned(),
            callback_path: "/oauth/callback/test".to_owned(),
            verifier: "verifier".to_owned(),
            signal: AbortController::new().signal(),
            callback_host: "127.0.0.1".to_owned(),
            settle: callback.settle.clone(),
            claimed: callback.claimed.clone(),
            shutdown: callback.shutdown.clone(),
        }
    }

    async fn callback_request(target: &str, handler: CallbackHandler) -> String {
        let (mut client, mut server) = tokio::io::duplex(16_384);
        client
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("request");
        let task = tokio::spawn(async move {
            handle_callback_connection(&mut server, handler).await;
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
        input: Result<String, AuthError>,
        auth_url: Mutex<Option<String>>,
        prompt_signal: Mutex<Option<Arc<dyn crate::types::AbortSignal>>>,
        events: Mutex<Vec<AuthEvent>>,
    }

    struct DeferredFetch {
        calls: Arc<AtomicUsize>,
        response: Mutex<Option<oneshot::Receiver<ProviderHttpResponse>>>,
    }

    impl FetchFunction for DeferredFetch {
        fn fetch(
            &self,
            _request: ProviderHttpRequest,
        ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let response = self
                .response
                .lock()
                .expect("deferred response")
                .take()
                .expect("one token exchange");
            Box::pin(async move {
                response
                    .await
                    .map_err(|_| "deferred response closed".to_owned())
            })
        }
    }

    impl AuthInteraction for ManualInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            if let AuthPrompt::ManualCode { signal, .. } = prompt {
                *self.prompt_signal.lock().expect("signal") = signal;
            }
            let input = self.input.clone();
            Box::pin(async move { input })
        }

        fn notify(&self, event: AuthEvent) {
            if let AuthEvent::AuthUrl { url, .. } = &event {
                *self.auth_url.lock().expect("URL") = Some(url.clone());
            }
            self.events.lock().expect("events").push(event);
        }
    }

    /// Ports pi `test/openrouter-oauth.test.ts:189` and `:222`.
    #[tokio::test]
    async fn manual_redirect_or_bare_code_mints_a_permanent_key() {
        for input in [
            "http://remote/callback?code=manual-code",
            "?code=manual-code",
            "  manual-code  ",
        ] {
            let captured = Arc::new(Mutex::new(None::<Value>));
            let body = captured.clone();
            let fetcher = fetch(move |request| {
                *body.lock().expect("body") = serde_json::from_slice(&request.body).ok();
                Ok(response(200, r#"{"key":"sk-or-manual"}"#))
            });
            let interaction = Arc::new(ManualInteraction {
                input: Ok(input.to_owned()),
                auth_url: Mutex::new(None),
                prompt_signal: Mutex::new(None),
                events: Mutex::new(Vec::new()),
            });
            let credential = (openrouter_oauth_with(
                fetcher,
                AUTHORIZE_URL.to_owned(),
                TOKEN_URL.to_owned(),
                Some("127.0.0.1".to_owned()),
                false,
            )
            .login)(crate::auth::helpers::normalize_interaction(
                interaction.clone(),
            ))
            .await
            .expect("credential");
            assert_eq!(credential.access, "sk-or-manual");
            assert_eq!(credential.expires, JS_MAX_SAFE_INTEGER);
            assert_eq!(
                captured.lock().expect("body").as_ref().expect("body")["code"],
                "manual-code"
            );
            let authorize = interaction
                .auth_url
                .lock()
                .expect("URL")
                .clone()
                .expect("URL");
            assert_eq!(
                url::Url::parse(&authorize)
                    .expect("URL")
                    .query_pairs()
                    .find(|(name, _)| name == "code_challenge_method")
                    .map(|(_, value)| value.into_owned())
                    .as_deref(),
                Some("S256")
            );
            assert!(
                interaction
                    .prompt_signal
                    .lock()
                    .expect("signal")
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
            );
        }
    }

    /// Ports pi `test/openrouter-oauth.test.ts:242` and `:258`.
    #[tokio::test]
    async fn cancelled_or_empty_manual_prompt_does_not_exchange() {
        for input in [
            Err(AuthError::new("Login cancelled")),
            Ok("   ".to_owned()),
            Ok("?code=".to_owned()),
            Ok("http://remote/callback?code=".to_owned()),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let fetcher = {
                let calls = calls.clone();
                fetch(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err("must not fetch".to_owned())
                })
            };
            let interaction = Arc::new(ManualInteraction {
                input,
                auth_url: Mutex::new(None),
                prompt_signal: Mutex::new(None),
                events: Mutex::new(Vec::new()),
            });
            let error = (openrouter_oauth_with(
                fetcher,
                AUTHORIZE_URL.to_owned(),
                TOKEN_URL.to_owned(),
                Some("127.0.0.1".to_owned()),
                false,
            )
            .login)(crate::auth::helpers::normalize_interaction(
                interaction.clone(),
            ))
            .await
            .expect_err("failure");
            assert!(matches!(
                error.message.as_str(),
                "Login cancelled" | "Missing authorization code"
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(!interaction.events.lock().expect("events").iter().any(
                |event| matches!(event, AuthEvent::Progress { message } if message.starts_with("Exchanging"))
            ));
        }
    }

    /// Pins pi `src/auth/oauth/openrouter.ts:52-67`.
    #[test]
    fn manual_code_parser_strips_a_leading_question_mark_and_rejects_empty_codes() {
        assert_eq!(
            parse_authorization_input("?code=authorization-code").as_deref(),
            Some("authorization-code")
        );
        for input in ["?code=", "http://remote/callback?code="] {
            assert_eq!(parse_authorization_input(input), None);
        }
    }

    /// Ports pi `test/openrouter-oauth.test.ts:167`.
    #[tokio::test]
    async fn successful_exchange_requires_a_key() {
        for body in [r#"{"user_id":"user-1"}"#, "null", "[]"] {
            let body = body.to_owned();
            let error = exchange_authorization_code(
                fetch(move |_| Ok(response(200, body.clone()))),
                TOKEN_URL.to_owned(),
                "code".to_owned(),
                "verifier".to_owned(),
                AbortController::new().signal(),
            )
            .await
            .expect_err("missing key");
            assert_eq!(
                error.message,
                "OpenRouter OAuth response carries no \"key\""
            );
        }
    }

    /// Pins pi `src/auth/oauth/openrouter.ts:143-193`.
    #[tokio::test]
    async fn callback_server_returns_route_and_request_validation_pages() {
        let fetcher = fetch(|_| Err("token exchange must not start".to_owned()));
        let mut callback =
            pending_callback_server("http://127.0.0.1/oauth/callback/test".to_owned());

        let not_found = callback_request(
            "/not-the-callback",
            callback_handler(&callback, fetcher.clone()),
        )
        .await;
        assert!(not_found.starts_with("HTTP/1.1 404 Not Found"));
        assert!(not_found.contains("OAuth callback route not found."));
        assert!(not_found.contains("Cache-Control: no-store"));

        let missing_code = callback_request(
            "/oauth/callback/test",
            callback_handler(&callback, fetcher.clone()),
        )
        .await;
        assert!(missing_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(missing_code.contains("OpenRouter returned no authorization code."));

        let empty_code = callback_request(
            "/oauth/callback/test?code=",
            callback_handler(&callback, fetcher.clone()),
        )
        .await;
        assert!(empty_code.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(empty_code.contains("OpenRouter returned no authorization code."));

        let denied = callback_request(
            "/oauth/callback/test?error=access_denied&error_description=user+denied",
            callback_handler(&callback, fetcher),
        )
        .await;
        assert!(denied.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(denied.contains("OpenRouter authorization was denied."));
        assert!(denied.contains("user denied"));
        assert_eq!(
            callback
                .wait_for_credential()
                .await
                .expect_err("denied login")
                .message,
            "OpenRouter authorization failed: user denied"
        );
        callback.close();
    }

    /// Ports pi `test/openrouter-oauth.test.ts:110`.
    #[tokio::test]
    async fn callback_exchange_failure_returns_502_and_rejects_the_waiter() {
        let fetcher = fetch(|_| Ok(response(403, r#"{"error":{"message":"invalid code"}}"#)));
        let mut callback =
            pending_callback_server("http://127.0.0.1/oauth/callback/test".to_owned());

        let page = callback_request(
            "/oauth/callback/test?code=bad-code",
            callback_handler(&callback, fetcher),
        )
        .await;
        assert!(page.starts_with("HTTP/1.1 502 Bad Gateway"));
        assert!(page.contains("OpenRouter key exchange failed."));
        assert!(page.contains("invalid code"));
        assert_eq!(
            callback
                .wait_for_credential()
                .await
                .expect_err("exchange failure")
                .message,
            "OpenRouter OAuth key exchange failed (HTTP 403): invalid code"
        );
        callback.close();
    }

    /// Ports pi `test/openrouter-oauth.test.ts:132`.
    #[tokio::test]
    async fn callback_claim_allows_only_one_exchange_then_returns_200() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (send_response, receive_response) = oneshot::channel();
        let fetcher: Arc<dyn FetchFunction> = Arc::new(DeferredFetch {
            calls: calls.clone(),
            response: Mutex::new(Some(receive_response)),
        });
        let mut callback =
            pending_callback_server("http://127.0.0.1/oauth/callback/test".to_owned());

        let first_callback = tokio::spawn(callback_request(
            "/oauth/callback/test?code=authorization-code",
            callback_handler(&callback, fetcher.clone()),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first exchange started");

        let duplicate = callback_request(
            "/oauth/callback/test?code=second-code",
            callback_handler(&callback, fetcher),
        )
        .await;
        assert!(duplicate.starts_with("HTTP/1.1 409 Conflict"));
        assert!(duplicate.contains("This OAuth callback has already been used."));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(
            send_response
                .send(response(200, r#"{"key":"sk-or-test"}"#))
                .is_ok()
        );
        let success = first_callback.await.expect("first callback");
        assert!(success.starts_with("HTTP/1.1 200 OK"));
        assert!(success.contains("Signed in to OpenRouter."));
        assert_eq!(
            callback
                .wait_for_credential()
                .await
                .expect("callback result")
                .expect("credential")
                .access,
            "sk-or-test"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        callback.close();
    }
}
