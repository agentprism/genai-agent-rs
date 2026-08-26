use agentprism_ai::{
    AuthAnswer, AuthError, AuthEvent, AuthHostCapabilities, AuthInteraction, AuthInteractionError,
    AuthPrompt, AuthResolutionOverrides, AuthSource, CancellationToken, Context, HttpRequest,
    HttpResponse, HttpTransport, LocalAuthInteraction, LocalBoxFuture, LocalHttpResponse,
    LocalHttpTransport, LocalModels, LocalOAuthAuth, LocalRedirectReceiver, ModelRequest, Models,
    OAuthAuth, OAuthCredential, ProviderOAuthExtra, RedirectArrival, RedirectReceiver,
    RedirectReceiverRequest, SecretString, SendBoxFuture, SimpleGenerationOptions, StreamTransport,
    Timestamp, TransportError,
};
use agentprism_openai::{OpenAiCodexResponsesTransport, openai_models, openai_provider};
use agentprism_openai_codex::{
    LocalOpenAiCodexOAuth, OpenAiCodexOAuth, account_id_from_jwt, local_openai_codex_provider,
    openai_codex_models, openai_codex_provider,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

struct UnusedTransport;

impl HttpTransport for UnusedTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { panic!("to_auth must not perform transport I/O") })
    }
}

struct AssertCodexTransport;

impl HttpTransport for AssertCodexTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        assert_eq!(
            request.url.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(request.headers[header::AUTHORIZATION], "Bearer access");
        assert_eq!(request.headers["chatgpt-account-id"], "account-123");
        assert_eq!(request.headers["originator"], "pi");
        assert_eq!(request.headers[header::USER_AGENT], "pi-ai-rs/0.1.0");
        assert_eq!(request.headers[header::ACCEPT], "text/event-stream");
        assert_eq!(request.headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(request.headers[header::CONTENT_ENCODING], "zstd");
        assert_eq!(request.headers["openai-beta"], "responses=experimental");
        assert_eq!(
            zstd::stream::decode_all(request.body.as_slice()).expect("zstd body"),
            Vec::<u8>::new()
        );
        Box::pin(async { Ok(HttpResponse::from_bytes(200, HeaderMap::new(), Vec::new())) })
    }
}

/// Architecture v2 part 2 §6.6; pinned Pi basis:
/// `packages/ai/src/auth/oauth/openai-codex.ts:getAccountId/toAuth` and
/// `packages/ai/src/api/openai-codex-responses.ts:buildBaseCodexHeaders`.
#[test]
fn openai_codex_oauth_derives_account_and_mandatory_headers() {
    let token = access_token("account-123");
    assert_eq!(account_id_from_jwt(&token).unwrap(), "account-123");
    let flow = OpenAiCodexOAuth::new(Arc::new(UnusedTransport));
    assert_eq!(flow.name(), "OpenAI (ChatGPT Plus/Pro)");
    let auth = futures_executor::block_on(flow.to_auth(&OAuthCredential {
        access: SecretString::new(token),
        refresh: SecretString::new("refresh"),
        expires_at: Timestamp::from_unix_millis(i64::MAX),
        extra: ProviderOAuthExtra::OpenAiCodex {
            account_id: "account-123".into(),
        },
    }))
    .expect("resolved Codex OAuth");
    assert_eq!(auth.source, AuthSource::new("OAuth"));
    assert_eq!(
        auth.headers[header::AUTHORIZATION],
        "Bearer ignored.header.signature".replace(
            "ignored.header.signature",
            auth.api_key.as_ref().unwrap().expose_secret()
        )
    );
    assert_eq!(auth.headers["chatgpt-account-id"], "account-123");
    assert_eq!(auth.headers["originator"], "pi");
    assert_eq!(auth.headers[header::USER_AGENT], "pi-ai-rs/0.1.0");
}

/// Architecture v2 part 2 §6.1/§6.6; pinned Pi basis:
/// `packages/ai/src/auth/oauth/openai-codex.ts:decodeJwt` requires exactly
/// three dot-separated JWT components before decoding the payload.
#[test]
fn openai_codex_oauth_rejects_non_three_part_jwt() {
    let valid = access_token("account-123");
    let mut parts = valid.split('.');
    let header = parts.next().unwrap();
    let payload = parts.next().unwrap();
    let signature = parts.next().unwrap();

    for malformed in [
        format!("{header}.{payload}"),
        format!("{header}.{payload}.{signature}.extra"),
    ] {
        let error = account_id_from_jwt(&malformed).expect_err("malformed JWT shape");
        assert_eq!(error.code(), "openai_codex_account");
        assert_eq!(error.to_string(), "Failed to extract accountId from token");
    }
}

#[derive(Default)]
struct BrowserTokenTransport {
    request_bodies: Mutex<Vec<Vec<u8>>>,
}

impl HttpTransport for BrowserTokenTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.request_bodies.lock().unwrap().push(request.body);
        let response = oauth_token_response();
        Box::pin(async move { Ok(response) })
    }
}

#[derive(Default)]
struct LocalBrowserTokenTransport {
    request_bodies: RefCell<Vec<Vec<u8>>>,
}

impl LocalHttpTransport for LocalBrowserTokenTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.request_bodies.borrow_mut().push(request.body);
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                oauth_token_body(),
            ))
        })
    }
}

struct StaticTokenTransport {
    status: u16,
    body: Vec<u8>,
}

impl HttpTransport for StaticTokenTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let response = HttpResponse::from_bytes(self.status, HeaderMap::new(), self.body.clone());
        Box::pin(async move { Ok(response) })
    }
}

struct LocalStaticTokenTransport {
    status: u16,
    body: Vec<u8>,
}

struct DeviceLoginHost;

impl AuthInteraction for DeviceLoginHost {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        assert!(matches!(prompt, AuthPrompt::Select { .. }));
        Box::pin(async { Ok(AuthAnswer::Selected("device_code".into())) })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        assert!(matches!(event, AuthEvent::DeviceCode { .. }));
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async { panic!("device login must not create a redirect receiver") })
    }
}

impl LocalAuthInteraction for DeviceLoginHost {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        assert!(matches!(prompt, AuthPrompt::Select { .. }));
        Box::pin(async { Ok(AuthAnswer::Selected("device_code".into())) })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        assert!(matches!(event, AuthEvent::DeviceCode { .. }));
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRedirectReceiver>, AuthInteractionError>> {
        Box::pin(async { panic!("device login must not create a redirect receiver") })
    }
}

struct DeviceSequenceTransport {
    responses: Mutex<VecDeque<(u16, Vec<u8>)>>,
}

impl DeviceSequenceTransport {
    fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

impl HttpTransport for DeviceSequenceTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let (status, body) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra device request");
        Box::pin(async move { Ok(HttpResponse::from_bytes(status, HeaderMap::new(), body)) })
    }
}

struct LocalDeviceSequenceTransport {
    responses: RefCell<VecDeque<(u16, Vec<u8>)>>,
}

impl LocalDeviceSequenceTransport {
    fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
        }
    }
}

impl LocalHttpTransport for LocalDeviceSequenceTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let (status, body) = self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("unexpected extra local device request");
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                status,
                HeaderMap::new(),
                body,
            ))
        })
    }
}

impl LocalHttpTransport for LocalStaticTokenTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let response =
            LocalHttpResponse::from_bytes(self.status, HeaderMap::new(), self.body.clone());
        Box::pin(async move { Ok(response) })
    }
}

/// Architecture v2 part 1 §3.8 and part 2 §6.1/§9.2; pinned Pi basis:
/// `openai-codex.ts:readTokenResponse` accepts every JSON number for
/// `expires_in`, including integral floating-point and fractional values.
#[test]
fn openai_codex_oauth_expires_in_accepts_json_number_domain_send_and_local() {
    let send_before = unix_now_millis();
    let send_flow = OpenAiCodexOAuth::new(Arc::new(StaticTokenTransport {
        status: 200,
        body: oauth_token_body_with_expires(serde_json::json!(3600.0)),
    }));
    let send = futures_executor::block_on(
        send_flow.refresh(seed_oauth_credential(), CancellationToken::new()),
    )
    .expect("Send refresh accepts an integral JSON float");
    let send_after = unix_now_millis();
    assert!(send.expires_at.unix_millis() >= send_before.saturating_add(3_600_000));
    assert!(send.expires_at.unix_millis() <= send_after.saturating_add(3_600_000));

    let local_before = unix_now_millis();
    let local_flow = LocalOpenAiCodexOAuth::new(Rc::new(LocalStaticTokenTransport {
        status: 200,
        body: oauth_token_body_with_expires(serde_json::json!(3600.25)),
    }));
    let local = futures_executor::block_on(
        local_flow.refresh(seed_oauth_credential(), CancellationToken::new()),
    )
    .expect("Local refresh accepts fractional seconds");
    let local_after = unix_now_millis();
    assert!(local.expires_at.unix_millis() >= local_before.saturating_add(3_600_250));
    assert!(local.expires_at.unix_millis() <= local_after.saturating_add(3_600_250));
}

/// Architecture v2 part 1 §3.8 and part 2 §6.1/§9.2; native AuthError
/// hardening at the pinned `openai-codex.ts:readTokenResponse` boundary.
#[test]
fn openai_codex_oauth_token_errors_redact_credentials_send_and_local() {
    let access = access_token("secret-account");
    let refresh = "secret-refresh-token";
    let credential_body = serde_json::to_vec(&serde_json::json!({
        "access_token": access,
        "refresh_token": refresh
    }))
    .unwrap();

    let send_flow = OpenAiCodexOAuth::new(Arc::new(StaticTokenTransport {
        status: 200,
        body: credential_body.clone(),
    }));
    let send_error = futures_executor::block_on(
        send_flow.refresh(seed_oauth_credential(), CancellationToken::new()),
    )
    .expect_err("missing expires_in must fail");
    assert_token_error_is_redacted(&send_error, &access, refresh);

    let local_flow = LocalOpenAiCodexOAuth::new(Rc::new(LocalStaticTokenTransport {
        status: 400,
        body: credential_body,
    }));
    let local_error = futures_executor::block_on(
        local_flow.refresh(seed_oauth_credential(), CancellationToken::new()),
    )
    .expect_err("non-success token response must fail");
    assert_token_error_is_redacted(&local_error, &access, refresh);
}

/// Architecture v2 part 1 §3.8 and part 2 §6.1/§6.6/§9.2; pinned Pi basis:
/// `openai-codex.ts:startOpenAICodexDeviceAuth/pollOpenAICodexDeviceAuth`,
/// with the native M3.3 sanitized `AuthError` boundary.
#[test]
fn openai_codex_device_malformed_responses_sanitize_secrets_send_and_local() {
    let cases = [
        (
            vec![(
                200,
                json_body(serde_json::json!({
                    "device_auth_id": "malformed-device-auth-secret"
                })),
            )],
            "malformed-device-auth-secret",
        ),
        (
            vec![
                valid_device_start_body("device-auth-secret"),
                (
                    200,
                    json_body(serde_json::json!({
                        "authorization_code": "malformed-authorization-secret"
                    })),
                ),
            ],
            "malformed-authorization-secret",
        ),
        (
            vec![
                valid_device_start_body("device-auth-secret"),
                (
                    200,
                    json_body(serde_json::json!({
                        "code_verifier": "malformed-verifier-secret"
                    })),
                ),
            ],
            "malformed-verifier-secret",
        ),
    ];

    for (responses, secret) in cases {
        let send_error = send_device_login_error(responses.clone());
        assert_device_error_is_redacted(&send_error, &[secret], None);

        let local_error = local_device_login_error(responses);
        assert_device_error_is_redacted(&local_error, &[secret], None);
    }
}

/// Architecture v2 part 1 §3.8 and part 2 §6.1/§6.6/§9.2; pinned Pi basis:
/// `openai-codex.ts:startOpenAICodexDeviceAuth/pollOpenAICodexDeviceAuth`
/// retains safe provider error details, while M3.3 forbids credential leakage.
#[test]
fn openai_codex_device_non_success_responses_sanitize_secrets_send_and_local() {
    let cases = [
        (
            vec![(
                500,
                json_body(serde_json::json!({
                    "error": "server_error",
                    "error_description": "try again later",
                    "device_auth_id": "failed-device-auth-secret"
                })),
            )],
            "failed-device-auth-secret",
        ),
        (
            vec![
                valid_device_start_body("device-auth-secret"),
                (
                    500,
                    json_body(serde_json::json!({
                        "error": "server_error",
                        "error_description": "try again later",
                        "authorization_code": "failed-authorization-secret",
                        "code_verifier": "failed-verifier-secret"
                    })),
                ),
            ],
            "failed-authorization-secret",
        ),
    ];

    for (responses, secret) in cases {
        let send_error = send_device_login_error(responses.clone());
        assert_device_error_is_redacted(
            &send_error,
            &[secret, "failed-verifier-secret"],
            Some("server_error"),
        );

        let local_error = local_device_login_error(responses);
        assert_device_error_is_redacted(
            &local_error,
            &[secret, "failed-verifier-secret"],
            Some("server_error"),
        );
    }
}

struct BrowserHost {
    opened_urls: Mutex<Vec<Url>>,
    omit_callback_state: bool,
}

impl BrowserHost {
    fn new(omit_callback_state: bool) -> Self {
        Self {
            opened_urls: Mutex::new(Vec::new()),
            omit_callback_state,
        }
    }
}

impl AuthInteraction for BrowserHost {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities {
            external_browser: true,
            loopback_http: true,
            ..Default::default()
        }
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        assert!(matches!(prompt, AuthPrompt::Select { .. }));
        Box::pin(async { Ok(AuthAnswer::Selected("browser".into())) })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        let AuthEvent::OpenUrl { url, .. } = event else {
            panic!("browser flow emitted an unexpected auth event")
        };
        self.opened_urls.lock().unwrap().push(url);
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        let mut callback = Url::parse("http://127.0.0.1:1455/auth/callback").unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "browser-code");
        if !self.omit_callback_state {
            callback
                .query_pairs_mut()
                .append_pair("state", request.challenge_id.as_str());
        }
        Box::pin(
            async move { Ok(Box::new(BrowserReceiver { callback }) as Box<dyn RedirectReceiver>) },
        )
    }
}

struct BrowserReceiver {
    callback: Url,
}

impl RedirectReceiver for BrowserReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.callback
    }

    fn receive(
        self: Box<Self>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            Ok(RedirectArrival {
                url: self.callback,
                received_at: Timestamp::from_unix_millis(1),
            })
        })
    }
}

struct LocalBrowserHost {
    opened_urls: RefCell<Vec<Url>>,
    omit_callback_state: bool,
}

impl LocalBrowserHost {
    fn new(omit_callback_state: bool) -> Self {
        Self {
            opened_urls: RefCell::new(Vec::new()),
            omit_callback_state,
        }
    }
}

impl LocalAuthInteraction for LocalBrowserHost {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities {
            external_browser: true,
            loopback_http: true,
            ..Default::default()
        }
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        assert!(matches!(prompt, AuthPrompt::Select { .. }));
        Box::pin(async { Ok(AuthAnswer::Selected("browser".into())) })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        let AuthEvent::OpenUrl { url, .. } = event else {
            panic!("browser flow emitted an unexpected auth event")
        };
        self.opened_urls.borrow_mut().push(url);
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRedirectReceiver>, AuthInteractionError>> {
        let mut callback = Url::parse("http://127.0.0.1:1455/auth/callback").unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "browser-code");
        if !self.omit_callback_state {
            callback
                .query_pairs_mut()
                .append_pair("state", request.challenge_id.as_str());
        }
        Box::pin(async move {
            Ok(Box::new(LocalBrowserReceiver { callback }) as Box<dyn LocalRedirectReceiver>)
        })
    }
}

struct LocalBrowserReceiver {
    callback: Url,
}

impl LocalRedirectReceiver for LocalBrowserReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.callback
    }

    fn receive(
        self: Box<Self>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            Ok(RedirectArrival {
                url: self.callback,
                received_at: Timestamp::from_unix_millis(1),
            })
        })
    }
}

/// Architecture v2 part 2 §6.1/§6.3/§9.2; pinned Pi basis:
/// `openai-codex.ts:loginWithBrowser` always advertises and exchanges the
/// registered localhost callback, even when the host binds 127.0.0.1.
#[test]
fn openai_codex_browser_login_uses_registered_redirect_uri_send_and_local() {
    let send_transport = Arc::new(BrowserTokenTransport::default());
    let send_host = Arc::new(BrowserHost::new(false));
    let send_flow = OpenAiCodexOAuth::new(send_transport.clone());
    let send_credential =
        futures_executor::block_on(send_flow.login(send_host.clone(), CancellationToken::new()))
            .expect("Send browser login");
    assert_eq!(
        send_credential.extra,
        ProviderOAuthExtra::OpenAiCodex {
            account_id: "oauth-account".into()
        }
    );
    assert_registered_redirect(&send_host.opened_urls.lock().unwrap()[0]);
    assert_exchange_redirect(&send_transport.request_bodies.lock().unwrap()[0]);

    let local_transport = Rc::new(LocalBrowserTokenTransport::default());
    let local_host = Rc::new(LocalBrowserHost::new(false));
    let local_flow = LocalOpenAiCodexOAuth::new(local_transport.clone());
    let local_credential =
        futures_executor::block_on(local_flow.login(local_host.clone(), CancellationToken::new()))
            .expect("Local browser login");
    assert_eq!(local_credential.extra, send_credential.extra);
    assert_registered_redirect(&local_host.opened_urls.borrow()[0]);
    assert_exchange_redirect(&local_transport.request_bodies.borrow()[0]);
}

/// Architecture v2 part 2 §6.1/§9.2; pinned Pi basis:
/// `openai-codex.ts:loginWithBrowser` requires the loopback callback state to
/// exactly match its generated state. Optional state applies only to manual
/// pasted raw codes.
#[test]
fn openai_codex_receiver_callback_requires_state_send_and_local() {
    let send_transport = Arc::new(BrowserTokenTransport::default());
    let send_host = Arc::new(BrowserHost::new(true));
    let send_flow = OpenAiCodexOAuth::new(send_transport.clone());
    let send_error =
        futures_executor::block_on(send_flow.login(send_host, CancellationToken::new()))
            .expect_err("Send receiver callback without state");
    assert!(matches!(send_error, AuthError::StateMismatch));
    assert!(send_transport.request_bodies.lock().unwrap().is_empty());

    let local_transport = Rc::new(LocalBrowserTokenTransport::default());
    let local_host = Rc::new(LocalBrowserHost::new(true));
    let local_flow = LocalOpenAiCodexOAuth::new(local_transport.clone());
    let local_error =
        futures_executor::block_on(local_flow.login(local_host, CancellationToken::new()))
            .expect_err("Local receiver callback without state");
    assert!(matches!(local_error, AuthError::StateMismatch));
    assert!(local_transport.request_bodies.borrow().is_empty());
}

/// Architecture v2 part 2 §2.6 correction; pinned Pi basis:
/// `openai-codex-responses.ts:buildSSEHeaders`.
#[test]
fn openai_codex_sse_transport_reasserts_protocol_headers() {
    let transport = OpenAiCodexResponsesTransport::new(Arc::new(AssertCodexTransport));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-overlay"),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("caller-account"),
    );
    headers.insert("originator", HeaderValue::from_static("caller"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static("caller"));
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    let mut auth_headers = HeaderMap::new();
    auth_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer access"),
    );
    auth_headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("account-123"),
    );
    auth_headers.insert("originator", HeaderValue::from_static("pi"));
    auth_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("pi-ai-rs/0.1.0"),
    );
    futures_executor::block_on(transport.execute(
        HttpRequest {
            method: Method::POST,
            url: Url::parse("https://chatgpt.com/backend-api").unwrap(),
            headers,
            auth_headers,
            session_id: None,
            body: Vec::new(),
            timeout: None,
            transport: Some(agentprism_ai::StreamTransport::Sse),
            websocket_connect_timeout: None,
            attempt: 0,
        },
        CancellationToken::new(),
    ))
    .expect("Codex transport");
}

/// Architecture v2 part 1 §3.6 and part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/providers/openai.ts` and `providers/openai-codex.ts`.
#[test]
fn openai_responses_provider_catalogs_match_pinned_families() {
    let openai = openai_models().expect("OpenAI catalog");
    let codex = openai_codex_models().expect("Codex catalog");
    assert!(!openai.is_empty());
    assert_eq!(codex.len(), 7);
    assert!(
        openai
            .iter()
            .all(|model| model.api.api_id().as_str() == "openai-responses")
    );
    assert!(
        codex
            .iter()
            .all(|model| model.api.api_id().as_str() == "openai-codex-responses")
    );
    assert!(
        codex
            .iter()
            .all(|model| model.common.model_ref.provider.as_str() == "openai-codex")
    );
    let provider = openai_provider(Arc::new(UnusedTransport)).expect("OpenAI provider");
    assert_eq!(provider.apis.len(), 1);
    assert!(
        provider
            .apis
            .contains_key(&agentprism_ai::ApiId::new("openai-responses"))
    );
}

#[derive(Default)]
struct AccessTokenTransport {
    headers: Mutex<Vec<HeaderMap>>,
}

impl HttpTransport for AccessTokenTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.headers.lock().unwrap().push(request.headers);
        Box::pin(async { Ok(codex_success_response()) })
    }
}

#[derive(Default)]
struct LocalAccessTokenTransport {
    headers: RefCell<Vec<HeaderMap>>,
}

impl LocalHttpTransport for LocalAccessTokenTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.headers.borrow_mut().push(request.headers);
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                codex_success_sse(),
            ))
        })
    }
}

/// Architecture v2 part 2 §6.6/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:streamOpenAICodexResponses` accepts a direct
/// access token, derives its account ID, and applies mandatory headers.
#[test]
fn openai_codex_explicit_access_token_auth_send_and_local() {
    let token = access_token("account-explicit");

    let send_transport = Arc::new(AccessTokenTransport::default());
    let send_model = openai_codex_models().unwrap()[0].common.model_ref.clone();
    let send_provider = openai_codex_provider(send_transport.clone()).unwrap();
    let send_models = Models::builder().provider(send_provider).build().unwrap();
    let send_stream = futures_executor::block_on(send_models.stream_simple_with_auth(
        codex_model_request(send_model),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new(token.clone())),
            ..Default::default()
        },
        CancellationToken::new(),
    ))
    .unwrap();
    futures_executor::block_on(async { send_stream.collect::<Vec<_>>().await });
    assert_access_token_headers(&send_transport.headers.lock().unwrap()[0], &token);

    let local_transport = Rc::new(LocalAccessTokenTransport::default());
    let local_model = openai_codex_models().unwrap()[0].common.model_ref.clone();
    let local_provider = local_openai_codex_provider(local_transport.clone()).unwrap();
    let local_models = LocalModels::builder()
        .provider(local_provider)
        .build()
        .unwrap();
    let local_stream = futures_executor::block_on(local_models.stream_simple_with_auth(
        codex_model_request(local_model),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new(token.clone())),
            ..Default::default()
        },
        CancellationToken::new(),
    ))
    .unwrap();
    futures_executor::block_on(async { local_stream.collect::<Vec<_>>().await });
    assert_access_token_headers(&local_transport.headers.borrow()[0], &token);
}

fn codex_model_request(model: agentprism_ai::ModelRef) -> ModelRequest {
    ModelRequest {
        model,
        context: Context::new(None),
        options: SimpleGenerationOptions {
            transport: Some(StreamTransport::Sse),
            ..Default::default()
        },
    }
}

fn assert_access_token_headers(headers: &HeaderMap, token: &str) {
    assert_eq!(
        headers[header::AUTHORIZATION],
        format!("Bearer {token}").as_str()
    );
    assert_eq!(headers["chatgpt-account-id"], "account-explicit");
    assert_eq!(headers["originator"], "pi");
    assert_eq!(headers[header::USER_AGENT], "pi-ai-rs/0.1.0");
}

fn codex_success_response() -> HttpResponse {
    HttpResponse::from_bytes(200, HeaderMap::new(), codex_success_sse())
}

fn codex_success_sse() -> Vec<u8> {
    br#"data: {"type":"response.completed","response":{"id":"resp_auth","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#
    .to_vec()
}

fn send_device_login_error(responses: Vec<(u16, Vec<u8>)>) -> AuthError {
    let flow = OpenAiCodexOAuth::new(Arc::new(DeviceSequenceTransport::new(responses)));
    futures_executor::block_on(flow.login(Arc::new(DeviceLoginHost), CancellationToken::new()))
        .expect_err("Send device login must reject the scripted response")
}

fn local_device_login_error(responses: Vec<(u16, Vec<u8>)>) -> AuthError {
    let flow = LocalOpenAiCodexOAuth::new(Rc::new(LocalDeviceSequenceTransport::new(responses)));
    futures_executor::block_on(flow.login(Rc::new(DeviceLoginHost), CancellationToken::new()))
        .expect_err("Local device login must reject the scripted response")
}

fn valid_device_start_body(device_auth_id: &str) -> (u16, Vec<u8>) {
    (
        200,
        json_body(serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": "ABCD-1234",
            "interval": 0
        })),
    )
}

fn json_body(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

fn assert_device_error_is_redacted(error: &AuthError, secrets: &[&str], safe_detail: Option<&str>) {
    for rendered in [error.to_string(), format!("{error:?}")] {
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "device credential leaked through AuthError: {rendered}"
            );
        }
        if let Some(safe_detail) = safe_detail {
            assert!(rendered.contains(safe_detail));
            assert!(rendered.contains("try again later"));
        } else {
            assert!(rendered.contains("redacted"));
        }
    }
}

fn oauth_token_response() -> HttpResponse {
    HttpResponse::from_bytes(200, HeaderMap::new(), oauth_token_body())
}

fn oauth_token_body() -> Vec<u8> {
    oauth_token_body_with_expires(serde_json::json!(3600))
}

fn oauth_token_body_with_expires(expires_in: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "access_token": access_token("oauth-account"),
        "refresh_token": "oauth-refresh",
        "expires_in": expires_in
    }))
    .unwrap()
}

fn seed_oauth_credential() -> OAuthCredential {
    OAuthCredential {
        access: SecretString::new("seed-access"),
        refresh: SecretString::new("seed-refresh"),
        expires_at: Timestamp::from_unix_millis(0),
        extra: ProviderOAuthExtra::OpenAiCodex {
            account_id: "seed-account".into(),
        },
    }
}

fn unix_now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn assert_token_error_is_redacted(error: &AuthError, access: &str, refresh: &str) {
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains(access));
        assert!(!rendered.contains(refresh));
        assert!(rendered.contains("redacted"));
    }
}

fn assert_registered_redirect(authorization_url: &Url) {
    let redirect_uri = authorization_url
        .query_pairs()
        .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned()));
    assert_eq!(
        redirect_uri.as_deref(),
        Some("http://localhost:1455/auth/callback")
    );
}

fn assert_exchange_redirect(body: &[u8]) {
    let redirect_uri = url::form_urlencoded::parse(body)
        .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned()));
    assert_eq!(
        redirect_uri.as_deref(),
        Some("http://localhost:1455/auth/callback")
    );
}

fn access_token(account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        }))
        .unwrap(),
    );
    format!("{header}.{payload}.signature")
}
