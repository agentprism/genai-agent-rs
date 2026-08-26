use agentprism_ai::*;
use agentprism_xai::{LocalXaiOAuth, XaiOAuth};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

#[path = "../../fixtures/oauth_support.rs"]
mod support;
use support::*;

fn credential() -> OAuthCredential {
    OAuthCredential {
        access: SecretString::new("old-access"),
        refresh: SecretString::new("old-refresh"),
        expires_at: Timestamp::default(),
        extra: ProviderOAuthExtra::None,
    }
}

fn device_response() -> HttpScriptedResponse {
    HttpScriptedResponse::json(
        200,
        r#"{"device_code":"device","user_code":"XAI-1","verification_uri":"https://auth.x.ai/device","interval":1,"expires_in":600}"#,
    )
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
fn xai_oauth_pi_exact_send_and_local() {
    // Pi basis: packages/ai/test/xai-oauth.test.ts and auth/oauth/xai.ts:
    // refresh-token retention, five-minute expiry skew, form encoding, and
    // HTTPS-only device verification links.
    let response =
        HttpScriptedResponse::json(200, r#"{"access_token":"new-access","expires_in":3600}"#);
    let send_transport = Arc::new(ScriptedTransport::new([response.clone()]));
    let oauth = XaiOAuth::new(send_transport.clone());
    let refreshed =
        futures_executor::block_on(oauth.refresh(credential(), CancellationToken::new())).unwrap();
    assert_eq!(refreshed.access.expose_secret(), "new-access");
    assert_eq!(refreshed.refresh.expose_secret(), "old-refresh");
    assert!(refreshed.expires_at.0 > Timestamp::default().0);
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(seen[0].url, "https://auth.x.ai/oauth2/token");
    let body = String::from_utf8_lossy(&seen[0].body);
    assert!(body.contains("grant_type=refresh_token"));
    assert!(body.contains("refresh_token=old-refresh"));
    drop(seen);

    let local_transport = Rc::new(ScriptedTransport::new([response]));
    let oauth = LocalXaiOAuth::new(local_transport.clone());
    let refreshed =
        futures_executor::block_on(oauth.refresh(credential(), CancellationToken::new())).unwrap();
    assert_eq!(refreshed.refresh.expose_secret(), "old-refresh");

    let unsafe_transport = Arc::new(ScriptedTransport::new([HttpScriptedResponse::json(
        200,
        r#"{"device_code":"device","user_code":"XAI-1","verification_uri":"http://auth.x.ai/device","interval":0,"expires_in":600}"#,
    )]));
    let error = futures_executor::block_on(XaiOAuth::new(unsafe_transport).login(
        Arc::new(RecordingInteraction::default()),
        CancellationToken::new(),
    ))
    .unwrap_err();
    assert!(error.to_string().contains("Untrusted verification URI"));
}

#[test]
fn xai_unknown_device_poll_failure_pi_exact_send_and_local() {
    // Pi basis: packages/ai/src/auth/oauth/xai.ts requestFailure() and
    // pollForTokens(): an unknown poll failure preserves HTTP status plus the
    // upstream error and error_description in both runtime families.
    let failure = HttpScriptedResponse::json(
        418,
        r#"{"error":"teapot","error_description":"device is cold"}"#,
    );
    let send_oauth = XaiOAuth::new(Arc::new(ScriptedTransport::new([
        device_response(),
        failure.clone(),
    ])));
    let local_oauth = LocalXaiOAuth::new(Rc::new(ScriptedTransport::new([
        device_response(),
        failure,
    ])));
    let send = send_oauth.login(
        Arc::new(RecordingInteraction::default()),
        CancellationToken::new(),
    );
    let local = local_oauth.login(
        Rc::new(RecordingInteraction::default()),
        CancellationToken::new(),
    );
    let (send, local) = futures_executor::block_on(futures_util::future::join(send, local));
    let expected = "xAI OAuth device token polling failed (HTTP 418): teapot: device is cold";
    assert_eq!(send.unwrap_err().to_string(), expected);
    assert_eq!(local.unwrap_err().to_string(), expected);
}

#[test]
fn xai_pending_oauth_body_is_cancellable_send_and_local() {
    // Pi basis: packages/ai/src/auth/oauth/xai.ts postForm() races fetch/body
    // consumption against AbortSignal for refresh as well as device login and
    // polling. Architecture basis: Part 2 §§6.1 and 9.5.
    let send_oauth = XaiOAuth::new(Arc::new(PendingBodyTransport));
    let send_cancellation = CancellationToken::new();
    let mut send = send_oauth.refresh(credential(), send_cancellation.clone());
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(send.as_mut().poll(&mut context), Poll::Pending));
    send_cancellation.cancel();
    assert_eq!(
        futures_executor::block_on(send).unwrap_err(),
        AuthError::Cancelled
    );

    let local_oauth = LocalXaiOAuth::new(Rc::new(PendingBodyTransport));
    let local_cancellation = CancellationToken::new();
    let mut local = local_oauth.refresh(credential(), local_cancellation.clone());
    let mut context = Context::from_waker(&waker);
    assert!(matches!(local.as_mut().poll(&mut context), Poll::Pending));
    local_cancellation.cancel();
    assert_eq!(
        futures_executor::block_on(local).unwrap_err(),
        AuthError::Cancelled
    );
}
