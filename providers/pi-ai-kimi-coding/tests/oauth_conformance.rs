use pi_ai::*;
use pi_ai_kimi_coding::*;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use url::Url;

#[path = "../../fixtures/oauth_support.rs"]
mod support;
use support::*;

fn device_response() -> HttpScriptedResponse {
    HttpScriptedResponse::json(
        200,
        r#"{"device_code":"device","user_code":"KIMI-1","verification_uri":"https://auth.kimi.test/device","verification_uri_complete":"https://auth.kimi.test/device?user_code=KIMI-1","interval":1,"expires_in":600}"#,
    )
}

fn responses() -> [HttpScriptedResponse; 2] {
    [
        device_response(),
        HttpScriptedResponse::json(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
    ]
}

#[derive(Clone, Copy)]
struct PendingBodyTransport;

impl HttpTransport for PendingBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

impl LocalHttpTransport for PendingBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

#[test]
fn kimi_coding_oauth_pi_exact_send_and_local() {
    // Pi basis: packages/ai/test/kimi-coding-oauth.test.ts and
    // auth/oauth/kimi-coding.ts: custom-host device/token endpoints, device
    // notification, and concrete Send/Local credential production.
    let production_send_transport = Arc::new(ScriptedTransport::new(responses()));
    let registration = provider(ProviderInputs {
        http: production_send_transport.clone(),
        environment: BTreeMap::from([
            (
                "KIMI_CODE_OAUTH_HOST".into(),
                "https://preferred.kimi.test/root/".into(),
            ),
            ("KIMI_OAUTH_HOST".into(), "https://legacy.kimi.test/".into()),
        ]),
    })
    .unwrap();
    let credential = futures_executor::block_on(registration.auth.login(
        Arc::new(RecordingInteraction::with_answers([AuthAnswer::Selected(
            "oauth".into(),
        )])),
        CancellationToken::new(),
    ))
    .unwrap();
    assert!(matches!(credential, Credential::OAuth(_)));
    let seen = production_send_transport.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://preferred.kimi.test/root/api/oauth/device_authorization"
    );
    assert_eq!(
        seen[1].url,
        "https://preferred.kimi.test/root/api/oauth/token"
    );
    drop(seen);

    let production_local_transport = Rc::new(ScriptedTransport::new(responses()));
    let registration = local_provider(LocalProviderInputs {
        http: production_local_transport.clone(),
        environment: BTreeMap::from([(
            "KIMI_OAUTH_HOST".into(),
            "https://legacy.kimi.test/".into(),
        )]),
    })
    .unwrap();
    let credential = futures_executor::block_on(registration.auth.login(
        Rc::new(RecordingInteraction::with_answers([AuthAnswer::Selected(
            "oauth".into(),
        )])),
        CancellationToken::new(),
    ))
    .unwrap();
    assert!(matches!(credential, Credential::OAuth(_)));
    assert_eq!(
        production_local_transport.seen.lock().unwrap()[0].url,
        "https://legacy.kimi.test/api/oauth/device_authorization"
    );

    let send_transport = Arc::new(ScriptedTransport::new(responses()));
    let send_interaction = Arc::new(RecordingInteraction::default());
    let oauth = KimiCodingOAuth::with_host(
        send_transport.clone(),
        Url::parse("https://auth.kimi.test/root/").unwrap(),
    );
    let credential =
        futures_executor::block_on(oauth.login(send_interaction.clone(), CancellationToken::new()))
            .unwrap();
    assert_eq!(credential.access.expose_secret(), "access");
    assert_eq!(credential.refresh.expose_secret(), "refresh");
    assert!(matches!(
        send_interaction.notifications.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode { user_code, .. }] if user_code == "KIMI-1"
    ));
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://auth.kimi.test/root/api/oauth/device_authorization"
    );
    assert_eq!(seen[1].url, "https://auth.kimi.test/root/api/oauth/token");
    assert!(String::from_utf8_lossy(&seen[1].body).contains("device_code=device"));
    drop(seen);

    let local_transport = Rc::new(ScriptedTransport::new(responses()));
    let local_interaction = Rc::new(RecordingInteraction::default());
    let oauth = LocalKimiCodingOAuth::with_host(
        local_transport.clone(),
        Url::parse("https://auth.kimi.test/root/").unwrap(),
    );
    let credential =
        futures_executor::block_on(oauth.login(local_interaction, CancellationToken::new()))
            .unwrap();
    assert_eq!(credential.access.expose_secret(), "access");
    assert_eq!(local_transport.seen.lock().unwrap().len(), 2);

    let send_transport = Arc::new(ScriptedTransport::new([
        HttpScriptedResponse::json(429, r#"{"error":"temporarily_unavailable"}"#),
        HttpScriptedResponse::json(
            200,
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":60}"#,
        ),
    ]));
    let oauth = KimiCodingOAuth::with_host(
        send_transport.clone(),
        Url::parse("https://auth.kimi.test/root/").unwrap(),
    );
    let refreshed = futures_executor::block_on(oauth.refresh(
        OAuthCredential {
            access: SecretString::new("old-access"),
            refresh: SecretString::new("old-refresh"),
            expires_at: Timestamp::default(),
            extra: ProviderOAuthExtra::None,
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(refreshed.access.expose_secret(), "new-access");
    assert_eq!(refreshed.refresh.expose_secret(), "new-refresh");
    assert_eq!(send_transport.seen.lock().unwrap().len(), 2);
}

#[test]
fn kimi_device_poll_failures_pi_exact_send_and_local() {
    // Pi basis: packages/ai/src/auth/oauth/kimi-coding.ts:167-205. The four
    // terminal poll branches retain Pi's expired/denied text, raw 5xx body,
    // and generic status/error/error_description message in real flows.
    let host = Url::parse("https://auth.kimi.test/root/").unwrap();
    let send_expired_oauth = KimiCodingOAuth::with_host(
        Arc::new(ScriptedTransport::new([
            device_response(),
            HttpScriptedResponse::json(400, r#"{"error":"expired_token"}"#),
        ])),
        host.clone(),
    );
    let local_denied_oauth = LocalKimiCodingOAuth::with_host(
        Rc::new(ScriptedTransport::new([
            device_response(),
            HttpScriptedResponse::json(400, r#"{"error":"access_denied"}"#),
        ])),
        host.clone(),
    );
    let send_server_oauth = KimiCodingOAuth::with_host(
        Arc::new(ScriptedTransport::new([
            device_response(),
            HttpScriptedResponse::json(503, "upstream unavailable"),
        ])),
        host.clone(),
    );
    let local_generic_oauth = LocalKimiCodingOAuth::with_host(
        Rc::new(ScriptedTransport::new([
            device_response(),
            HttpScriptedResponse::json(
                409,
                r#"{"error":"invalid_request","error_description":"device mismatch"}"#,
            ),
        ])),
        host,
    );

    let results = futures_executor::block_on(futures_util::future::join4(
        send_expired_oauth.login(
            Arc::new(RecordingInteraction::default()),
            CancellationToken::new(),
        ),
        local_denied_oauth.login(
            Rc::new(RecordingInteraction::default()),
            CancellationToken::new(),
        ),
        send_server_oauth.login(
            Arc::new(RecordingInteraction::default()),
            CancellationToken::new(),
        ),
        local_generic_oauth.login(
            Rc::new(RecordingInteraction::default()),
            CancellationToken::new(),
        ),
    ));

    assert_eq!(
        results.0.unwrap_err().to_string(),
        "Kimi Code device authorization expired. Please restart login."
    );
    assert_eq!(
        results.1.unwrap_err().to_string(),
        "Kimi Code login was denied."
    );
    assert_eq!(
        results.2.unwrap_err().to_string(),
        "Kimi Code device token request failed with status 503: upstream unavailable"
    );
    assert_eq!(
        results.3.unwrap_err().to_string(),
        "Kimi Code device token request failed (status 409): invalid_request: device mismatch"
    );
}

#[test]
fn kimi_pending_oauth_body_is_cancellable_send_and_local() {
    // Pi basis: packages/ai/src/auth/oauth/kimi-coding.ts requestSignal() and
    // its device/poll/refresh body reads. Architecture basis: Part 2 §§6.1
    // and 9.5 require whole-flow cancellation for Send and Local hosts.
    let host = Url::parse("https://auth.kimi.test/root/").unwrap();
    let send_oauth = KimiCodingOAuth::with_host(Arc::new(PendingBodyTransport), host.clone());
    let send_cancellation = CancellationToken::new();
    let mut send = send_oauth.login(
        Arc::new(RecordingInteraction::default()),
        send_cancellation.clone(),
    );
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(send.as_mut().poll(&mut context), Poll::Pending));
    send_cancellation.cancel();
    assert_eq!(
        futures_executor::block_on(send).unwrap_err(),
        AuthError::Cancelled
    );

    let local_oauth = LocalKimiCodingOAuth::with_host(Rc::new(PendingBodyTransport), host);
    let local_cancellation = CancellationToken::new();
    let mut local = local_oauth.login(
        Rc::new(RecordingInteraction::default()),
        local_cancellation.clone(),
    );
    let mut context = Context::from_waker(&waker);
    assert!(matches!(local.as_mut().poll(&mut context), Poll::Pending));
    local_cancellation.cancel();
    assert_eq!(
        futures_executor::block_on(local).unwrap_err(),
        AuthError::Cancelled
    );
}
