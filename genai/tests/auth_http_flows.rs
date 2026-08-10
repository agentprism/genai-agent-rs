//! Offline HTTP-flow tests for the Codex OAuth token exchange, refresh, and
//! device-code polling.
//!
//! These use a local one-shot mock HTTP server (a tokio `TcpListener` returning
//! canned JSON), never the real `auth.openai.com`. The mock captures each request
//! so we can assert the exact URL path, method, headers, and body fields.
#![cfg(feature = "auth")]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use genai::auth::codex::{CodexAuth, CodexConfig};
use genai::auth::{CredentialStore, FileCredentialStore, OAuthCredential};

// ---------------------------------------------------------------------------
// Mock HTTP server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl CapturedRequest {
    fn form_pairs(&self) -> HashMap<String, String> {
        reqwest::Url::parse(&format!("http://x/?{}", self.body))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Debug, Clone)]
struct MockResponse {
    status: u16,
    content_type: String,
    body: String,
}

impl MockResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json".to_string(),
            body: body.into(),
        }
    }
}

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Arc<Mutex<HashMap<String, VecDeque<MockResponse>>>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let responses: Arc<Mutex<HashMap<String, VecDeque<MockResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let req_clone = requests.clone();
        let resp_clone = responses.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let requests = req_clone.clone();
                let responses = resp_clone.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(socket, requests, responses).await;
                });
            }
        });

        Self {
            base_url,
            requests,
            responses,
        }
    }

    /// Queue a response for the given path (FIFO per path).
    fn push(&self, path: &str, response: MockResponse) {
        self.responses
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default()
            .push_back(response);
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn requests_for(&self, path: &str) -> Vec<CapturedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }
}

async fn handle_conn(
    mut socket: TcpStream,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Arc<Mutex<HashMap<String, VecDeque<MockResponse>>>>,
) -> std::io::Result<()> {
    let (head, body) = read_request(&mut socket).await?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let path = target.split('?').next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    requests.lock().unwrap().push(CapturedRequest {
        method,
        path: path.clone(),
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    });

    let response = responses
        .lock()
        .unwrap()
        .get_mut(&path)
        .and_then(|q| q.pop_front())
        .unwrap_or_else(|| MockResponse::json(200, "{}"));

    let wire = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.content_type,
        response.body.len(),
        response.body
    );
    socket.write_all(wire.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];

    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok((String::from_utf8_lossy(&buf).to_string(), Vec::new()));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    Ok((head, body))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an unsigned JWT whose payload carries the ChatGPT account id claim.
fn jwt_with_account_id(account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": account_id },
        "sub": "user_1",
    });
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    let sig = URL_SAFE_NO_PAD.encode(b"sig");
    format!("{header}.{body}.{sig}")
}

fn codex_for(server: &MockServer) -> CodexAuth {
    CodexAuth::with_config(CodexConfig {
        base_url: server.base_url.clone(),
        ..CodexConfig::default()
    })
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authorization_code_exchange_builds_correct_request() {
    let server = MockServer::start().await;
    let access = jwt_with_account_id("acct_exchange");
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": access,
                "refresh_token": "refresh-new",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = codex_for(&server);
    let before = genai::auth::credential::now_unix_ms();
    let cred = auth
        .exchange_authorization_code(
            "the-code",
            "the-verifier",
            "http://localhost:1455/auth/callback",
        )
        .await
        .expect("exchange should succeed");
    let after = genai::auth::credential::now_unix_ms();

    // Credential fields.
    assert_eq!(cred.access_token, access);
    assert_eq!(cred.refresh_token.as_deref(), Some("refresh-new"));
    assert_eq!(cred.account_id.as_deref(), Some("acct_exchange"));
    let exp = cred.expires_at_ms.unwrap();
    assert!(
        exp >= before + 3_600_000 && exp <= after + 3_600_000,
        "expiry off: {exp}"
    );

    // Request construction.
    let reqs = server.requests_for("/oauth/token");
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/oauth/token");
    assert_eq!(
        req.headers.get("content-type").map(String::as_str),
        Some("application/x-www-form-urlencoded")
    );
    let form = req.form_pairs();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some(genai::auth::codex::CLIENT_ID)
    );
    assert_eq!(form.get("code").map(String::as_str), Some("the-code"));
    assert_eq!(
        form.get("code_verifier").map(String::as_str),
        Some("the-verifier")
    );
    assert_eq!(
        form.get("redirect_uri").map(String::as_str),
        Some("http://localhost:1455/auth/callback")
    );
}

#[tokio::test]
async fn token_exchange_surfaces_http_error() {
    let server = MockServer::start().await;
    server.push(
        "/oauth/token",
        MockResponse::json(400, r#"{"error":"invalid_grant"}"#),
    );

    let auth = codex_for(&server);
    let err = auth
        .exchange_authorization_code("c", "v", "http://localhost:1455/auth/callback")
        .await
        .unwrap_err();
    match err {
        genai::auth::Error::TokenRequest {
            operation,
            status,
            body,
        } => {
            assert_eq!(operation, "exchange");
            assert_eq!(status, 400);
            assert!(body.contains("invalid_grant"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn missing_account_id_is_rejected_by_default() {
    let server = MockServer::start().await;
    // access token with no chatgpt_account_id claim.
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"u"}"#);
    let access = format!("{header}.{payload}.{}", URL_SAFE_NO_PAD.encode(b"s"));
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({ "access_token": access, "refresh_token": "r", "expires_in": 10 })
                .to_string(),
        ),
    );

    let auth = codex_for(&server);
    let err = auth
        .exchange_authorization_code("c", "v", "http://localhost:1455/auth/callback")
        .await
        .unwrap_err();
    assert!(
        matches!(err, genai::auth::Error::MissingAccountId),
        "got {err:?}"
    );
}

#[tokio::test]
async fn empty_string_tokens_are_rejected_as_missing_fields() {
    let server = MockServer::start().await;
    // Empty access_token (pi treats "" as falsy => missing).
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({ "access_token": "", "refresh_token": "r", "expires_in": 3600 })
                .to_string(),
        ),
    );
    // Empty refresh_token (second call pops this response).
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": jwt_with_account_id("acct_x"),
                "refresh_token": "",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = codex_for(&server);

    for _ in 0..2 {
        let err = auth
            .exchange_authorization_code("c", "v", "http://localhost:1455/auth/callback")
            .await
            .unwrap_err();
        match err {
            genai::auth::Error::TokenResponseMissingFields { operation, .. } => {
                assert_eq!(operation, "exchange");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Refresh (and store update)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_builds_correct_request_and_updates_store() {
    let server = MockServer::start().await;
    let new_access = jwt_with_account_id("acct_refresh");
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": new_access,
                "refresh_token": "refresh-rotated",
                "expires_in": 7200,
            })
            .to_string(),
        ),
    );

    let auth = codex_for(&server);

    // Seed the store with an expired credential.
    let dir = tempfile::tempdir().unwrap();
    let store = FileCredentialStore::new(dir.path().join("auth.json"));
    let stale = OAuthCredential::new(
        jwt_with_account_id("acct_old"),
        Some("refresh-old".into()),
        Some(1), // long past
        Some("acct_old".into()),
    );
    store
        .store(genai::auth::OPENAI_CODEX_PROVIDER_ID, &stale)
        .unwrap();

    // Refresh and persist.
    let refreshed = auth.refresh(&stale).await.expect("refresh should succeed");
    store
        .store(genai::auth::OPENAI_CODEX_PROVIDER_ID, &refreshed)
        .unwrap();

    assert_eq!(refreshed.access_token, new_access);
    assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-rotated"));
    assert_eq!(refreshed.account_id.as_deref(), Some("acct_refresh"));

    // The store now reflects the rotated credential.
    let loaded = store
        .load(genai::auth::OPENAI_CODEX_PROVIDER_ID)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.access_token, new_access);
    assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-rotated"));

    // Request construction.
    let reqs = server.requests_for("/oauth/token");
    assert_eq!(reqs.len(), 1);
    let form = reqs[0].form_pairs();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("refresh-old")
    );
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some(genai::auth::codex::CLIENT_ID)
    );
    assert_eq!(
        reqs[0].headers.get("content-type").map(String::as_str),
        Some("application/x-www-form-urlencoded")
    );
}

#[tokio::test]
async fn refresh_without_refresh_token_errors() {
    let server = MockServer::start().await;
    let auth = codex_for(&server);
    let cred = OAuthCredential::new("acc", None, Some(1), None);
    let err = auth.refresh(&cred).await.unwrap_err();
    assert!(
        matches!(err, genai::auth::Error::MissingRefreshToken),
        "got {err:?}"
    );
    // No network call should have been made.
    assert!(server.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Device-code flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_code_polls_until_authorized() {
    let server = MockServer::start().await;

    // usercode: a tiny interval (clamped to 1s minimum by the loop).
    server.push(
        "/api/accounts/deviceauth/usercode",
        MockResponse::json(
            200,
            serde_json::json!({
                "device_auth_id": "dev-123",
                "user_code": "WXYZ-1234",
                "interval": 0.01,
            })
            .to_string(),
        ),
    );

    // device token: authorization_pending, then success.
    server.push(
        "/api/accounts/deviceauth/token",
        MockResponse::json(400, r#"{"error":"deviceauth_authorization_pending"}"#),
    );
    server.push(
        "/api/accounts/deviceauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "authorization_code": "AUTH-CODE",
                "code_verifier": "SERVER-VERIFIER",
            })
            .to_string(),
        ),
    );

    // exchange (device redirect uri).
    let access = jwt_with_account_id("acct_device");
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": access,
                "refresh_token": "refresh-device",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = codex_for(&server);

    let begin = auth.begin_device_login().await.expect("begin device login");
    assert_eq!(begin.device_auth_id, "dev-123");
    assert_eq!(begin.user_code, "WXYZ-1234");
    assert!((begin.interval_seconds - 0.01).abs() < 1e-9);
    assert_eq!(
        begin.verification_uri,
        format!("{}/codex/device", server.base_url)
    );
    assert_eq!(begin.expires_in_seconds, 15 * 60);

    // usercode request was JSON with client_id.
    let usercode_reqs = server.requests_for("/api/accounts/deviceauth/usercode");
    assert_eq!(usercode_reqs.len(), 1);
    assert_eq!(
        usercode_reqs[0]
            .headers
            .get("content-type")
            .map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        usercode_reqs[0].json()["client_id"],
        genai::auth::codex::CLIENT_ID
    );

    let cred = auth
        .poll_device_login(&begin)
        .await
        .expect("poll device login");
    assert_eq!(cred.access_token, access);
    assert_eq!(cred.refresh_token.as_deref(), Some("refresh-device"));
    assert_eq!(cred.account_id.as_deref(), Some("acct_device"));

    // The device-token endpoint was polled at least twice (pending, then success).
    let token_reqs = server.requests_for("/api/accounts/deviceauth/token");
    assert!(
        token_reqs.len() >= 2,
        "expected >=2 polls, got {}",
        token_reqs.len()
    );
    assert_eq!(token_reqs[0].json()["device_auth_id"], "dev-123");
    assert_eq!(token_reqs[0].json()["user_code"], "WXYZ-1234");

    // The follow-up exchange used the device redirect uri and server verifier.
    let exchange_reqs = server.requests_for("/oauth/token");
    assert_eq!(exchange_reqs.len(), 1);
    let form = exchange_reqs[0].form_pairs();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(form.get("code").map(String::as_str), Some("AUTH-CODE"));
    assert_eq!(
        form.get("code_verifier").map(String::as_str),
        Some("SERVER-VERIFIER")
    );
    assert_eq!(
        form.get("redirect_uri").map(String::as_str),
        Some(format!("{}/deviceauth/callback", server.base_url).as_str())
    );
}

#[tokio::test]
async fn device_code_not_enabled_maps_404() {
    let server = MockServer::start().await;
    server.push(
        "/api/accounts/deviceauth/usercode",
        MockResponse::json(404, "not found"),
    );
    let auth = codex_for(&server);
    let err = auth.begin_device_login().await.unwrap_err();
    assert!(
        matches!(err, genai::auth::Error::DeviceCodeNotEnabled),
        "got {err:?}"
    );
}

#[tokio::test]
async fn complete_browser_login_validates_state_and_exchanges() {
    let server = MockServer::start().await;
    let access = jwt_with_account_id("acct_browser");
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": access,
                "refresh_token": "refresh-browser",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = codex_for(&server);
    let pending = auth.begin_browser_login().unwrap();

    // Wrong state is rejected before any network call.
    let bad = auth
        .complete_browser_login(
            &pending,
            &format!("http://localhost/cb?code=c&state={}", "WRONG"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(bad, genai::auth::Error::StateMismatch),
        "got {bad:?}"
    );
    assert!(server.requests().is_empty());

    // Correct state exchanges.
    let redirect = format!(
        "http://localhost:1455/auth/callback?code=THE-CODE&state={}",
        pending.state
    );
    let cred = auth
        .complete_browser_login(&pending, &redirect)
        .await
        .unwrap();
    assert_eq!(cred.access_token, access);
    assert_eq!(cred.account_id.as_deref(), Some("acct_browser"));

    let form = server.requests_for("/oauth/token")[0].form_pairs();
    assert_eq!(form.get("code").map(String::as_str), Some("THE-CODE"));
    assert_eq!(
        form.get("code_verifier").map(String::as_str),
        Some(pending.verifier.as_str())
    );
}

// ---------------------------------------------------------------------------
// genai resolver (feature = "genai")
// ---------------------------------------------------------------------------

#[cfg(feature = "auth")]
#[tokio::test]
async fn genai_resolver_refreshes_and_persists() {
    use std::sync::Arc;
    use std::time::Duration;

    let server = MockServer::start().await;
    let new_access = jwt_with_account_id("acct_resolved");
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": new_access,
                "refresh_token": "refresh-2",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = Arc::new(codex_for(&server));
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));

    // Seed an expired credential.
    let stale = OAuthCredential::new(
        jwt_with_account_id("acct_old"),
        Some("refresh-1".into()),
        Some(1),
        Some("acct_old".into()),
    );
    store
        .store(genai::auth::OPENAI_CODEX_PROVIDER_ID, &stale)
        .unwrap();

    // Build the resolver (checks the genai types line up) ...
    let _resolver = genai::auth::genai_integration::codex_auth_resolver(
        auth.clone(),
        store.clone(),
        genai::auth::OPENAI_CODEX_PROVIDER_ID,
    );

    // ... and exercise the underlying refresh-and-persist building block.
    let token = genai::auth::genai_integration::resolve_access_token(
        &auth,
        store.as_ref(),
        genai::auth::OPENAI_CODEX_PROVIDER_ID,
        Duration::from_secs(0),
    )
    .await
    .unwrap();

    assert_eq!(token, new_access);
    let loaded = store
        .load(genai::auth::OPENAI_CODEX_PROVIDER_ID)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.access_token, new_access);
    assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-2"));
}

#[cfg(feature = "auth")]
#[tokio::test]
async fn concurrent_resolvers_refresh_only_once() {
    use std::sync::Arc;
    use std::time::Duration;

    use genai::auth::genai_integration::CodexTokenResolver;

    let server = MockServer::start().await;
    let new_access = jwt_with_account_id("acct_concurrent");
    // Queue EXACTLY ONE refresh response. If a second refresh were issued it
    // would fall through to the mock's default "{}" and fail the resolve, so the
    // "both succeed + exactly one hit" assertions below prove single-refresh.
    server.push(
        "/oauth/token",
        MockResponse::json(
            200,
            serde_json::json!({
                "access_token": new_access,
                "refresh_token": "refresh-rotated",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    );

    let auth = Arc::new(codex_for(&server));
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));

    // Seed an expired credential with a (rotating) refresh token.
    let stale = OAuthCredential::new(
        jwt_with_account_id("acct_old"),
        Some("refresh-old".into()),
        Some(1),
        Some("acct_old".into()),
    );
    store
        .store(genai::auth::OPENAI_CODEX_PROVIDER_ID, &stale)
        .unwrap();

    // One shared resolver instance (single mutex) — exactly what the genai
    // resolver captures internally.
    let resolver = Arc::new(CodexTokenResolver::with_skew(
        auth.clone(),
        store.clone(),
        genai::auth::OPENAI_CODEX_PROVIDER_ID,
        Duration::from_secs(0),
    ));

    // Fire two resolves concurrently.
    let t1 = {
        let r = resolver.clone();
        tokio::spawn(async move { r.resolve().await })
    };
    let t2 = {
        let r = resolver.clone();
        tokio::spawn(async move { r.resolve().await })
    };
    let (a, b) = tokio::join!(t1, t2);
    let a = a.unwrap().expect("resolve 1 should succeed");
    let b = b.unwrap().expect("resolve 2 should succeed");

    // Both observed the same freshly rotated token ...
    assert_eq!(a, new_access);
    assert_eq!(b, new_access);

    // ... and the token endpoint was hit EXACTLY once (no double refresh race).
    let hits = server.requests_for("/oauth/token");
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one refresh request, got {}",
        hits.len()
    );
    let form = hits[0].form_pairs();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("refresh-old")
    );

    // The store now holds the rotated credential.
    let loaded = store
        .load(genai::auth::OPENAI_CODEX_PROVIDER_ID)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.access_token, new_access);
    assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-rotated"));
}
