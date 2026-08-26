//! Cloudflare endpoint/auth conformance.

use agentprism_ai::*;
use agentprism_cloudflare_ai_gateway::*;
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

#[path = "../../fixtures/oauth_support.rs"]
mod support;
use support::RecordingInteraction;

/// Architecture v2 part 2 §5.1 and §10.7; pinned Pi basis:
/// `packages/ai/scripts/generate-models.ts` and
/// `packages/ai/src/providers/cloudflare-ai-gateway.ts`.
#[test]
fn cloudflare_gateway_catalog_mirrors_workers_ai_tool_models_pi_exact() {
    let catalog = models().expect("Cloudflare AI Gateway catalog");
    assert_eq!(catalog.len(), 60);
    for id in [
        "workers-ai/@cf/deepseek-ai/deepseek-v4-flash-0731",
        "workers-ai/@cf/deepseek-ai/deepseek-v4-pro-0813",
        "workers-ai/@cf/qwen/qwen3.8-27b",
    ] {
        let model = catalog
            .iter()
            .find(|model| model.common.model_ref.model.as_str() == id)
            .unwrap_or_else(|| panic!("missing mirrored Workers AI model {id}"));
        assert_eq!(
            model.common.base_url.as_str(),
            "https://gateway.ai.cloudflare.com/v1/%7BCLOUDFLARE_ACCOUNT_ID%7D/%7BCLOUDFLARE_GATEWAY_ID%7D/compat"
        );
        let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
            panic!("mirrored Workers AI model {id} must use OpenAI Completions")
        };
        assert_eq!(config.compat.send_session_affinity_headers, Some(true));
    }
}

#[derive(Clone)]
struct NoNetwork;

impl HttpTransport for NoNetwork {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("no_network", "not used")) })
    }
}

impl LocalHttpTransport for NoNetwork {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("no_network", "not used")) })
    }
}

fn stored_cloudflare_credential() -> Credential {
    Credential::ApiKey(ApiKeyCredential {
        key: Some(SecretString::new("stored-key")),
        environment: BTreeMap::from([
            ("CLOUDFLARE_ACCOUNT_ID".into(), "stored-account".into()),
            ("CLOUDFLARE_GATEWAY_ID".into(), "stored-gateway".into()),
        ]),
    })
}

fn request_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("CLOUDFLARE_ACCOUNT_ID".into(), "request-account".into()),
        ("CLOUDFLARE_GATEWAY_ID".into(), "request-gateway".into()),
    ])
}

mod send_precedence {
    use super::*;

    #[test]
    fn auth_explicit_request_value_wins() {
        // Architecture v2 part 2 §6.1/§10.7; Pi basis: models.ts resolves
        // `{ ...stored.env, ...overrides.env }` before provider auth.
        let registration = provider(ProviderInputs {
            http: Arc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .unwrap();
        let store = Arc::new(InMemoryCredentialStore::new());
        futures_executor::block_on(async {
            let mut lease = store
                .acquire_lease(
                    ProviderId::new("cloudflare-ai-gateway"),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            lease.replace(Some(stored_cloudflare_credential()));
            lease.commit().await.unwrap();
        });
        let mut request = ResolveAuthRequest::isolated(
            registration.descriptor.clone(),
            Some(registration.catalog.snapshot()[0].clone()),
        );
        request.credential_store = store;
        request.overrides.environment = request_environment();
        let resolved = futures_executor::block_on(
            registration.auth.resolve(request, CancellationToken::new()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved.base_url.unwrap().as_str(),
            "https://gateway.ai.cloudflare.com/v1/request-account/request-gateway/anthropic"
        );
    }
}

mod local_precedence {
    use super::*;

    #[test]
    fn auth_explicit_request_value_wins() {
        // Local §9.2 counterpart to the Send conformance test above.
        let registration = local_provider(LocalProviderInputs {
            http: Rc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .unwrap();
        let store = Rc::new(LocalInMemoryCredentialStore::new());
        futures_executor::block_on(async {
            let mut lease = LocalCredentialStore::acquire_lease(
                store.as_ref(),
                ProviderId::new("cloudflare-ai-gateway"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            lease.replace(Some(stored_cloudflare_credential()));
            lease.commit().await.unwrap();
        });
        let mut request = LocalResolveAuthRequest::isolated(
            registration.descriptor.clone(),
            Some(registration.catalog.snapshot()[0].clone()),
        );
        request.credential_store = store;
        request.overrides.environment = request_environment();
        let resolved = futures_executor::block_on(
            registration.auth.resolve(request, CancellationToken::new()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved.base_url.unwrap().as_str(),
            "https://gateway.ai.cloudflare.com/v1/request-account/request-gateway/anthropic"
        );
    }
}

#[test]
fn cloudflare_stream_materializes_endpoint_and_auth_pi_exact() {
    // Pi basis: packages/ai/test/cloudflare-stream.test.ts and
    // providers/cloudflare-auth.ts per-field resolution.
    let registration = provider(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();
    let auth_context = MapAuthContext::new(
        BTreeMap::from([
            ("CLOUDFLARE_API_KEY".into(), "secret".into()),
            ("CLOUDFLARE_ACCOUNT_ID".into(), "account".into()),
            ("CLOUDFLARE_GATEWAY_ID".into(), "gateway".into()),
        ]),
        [],
    );
    let mut request = ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model));
    request.auth_context = Arc::new(auth_context);
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://gateway.ai.cloudflare.com/v1/account/gateway/anthropic"
    );
    assert_eq!(
        resolved.headers.get("cf-aig-authorization").unwrap(),
        "Bearer secret"
    );
    assert!(!resolved.headers.contains_key(http::header::AUTHORIZATION));
}

#[derive(Clone, Default)]
struct CaptureCloudflareRequests {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl HttpTransport for CaptureCloudflareRequests {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let is_completions = request.url.path().ends_with("/chat/completions");
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if is_completions {
                Ok(HttpResponse::from_bytes(
                    200,
                    http::HeaderMap::new(),
                    br#"data: {"id":"chat-cf","model":"workers-ai/@cf/moonshotai/kimi-k2.6","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}

data: [DONE]

"#
                    .to_vec(),
                ))
            } else {
                Ok(HttpResponse::from_bytes(
                    400,
                    http::HeaderMap::new(),
                    b"captured".to_vec(),
                ))
            }
        })
    }
}

fn cloudflare_context(system_prompt: Option<&str>) -> Context {
    let mut context = Context::new(system_prompt.map(str::to_owned));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("cloudflare-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("cloudflare-text"),
            text: "hi".into(),
        }],
        timestamp: Timestamp::from_unix_millis(1),
    }));
    context
}

#[test]
fn cloudflare_openai_completions_empty_tools_scenarios_pi_exact() {
    // Architecture v2 part 2 §3.3, §3.6, §6.1, and §10.8; Pi basis:
    // packages/ai/test/openai-completions-empty-tools.test.ts, Cloudflare
    // conservative fields, endpoint/auth, BYOK, and Workers AI affinity.
    let capture = Arc::new(CaptureCloudflareRequests::default());
    let registration = provider(ProviderInputs {
        http: capture.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let catalog = registration.catalog.snapshot();
    let workers = catalog
        .iter()
        .find(|model| {
            model.common.model_ref.model.as_str() == "workers-ai/@cf/moonshotai/kimi-k2.6"
        })
        .expect("Workers AI Kimi model")
        .clone();
    let byok = catalog
        .iter()
        .find(|model| model.common.model_ref.model.as_str() == "gpt-5.1")
        .expect("Cloudflare BYOK model")
        .clone();
    let models = Models::builder()
        .provider(registration)
        .auth_context(Arc::new(MapAuthContext::new(
            BTreeMap::from([
                ("CLOUDFLARE_API_KEY".into(), "cf-token".into()),
                ("CLOUDFLARE_ACCOUNT_ID".into(), "account-id".into()),
                ("CLOUDFLARE_GATEWAY_ID".into(), "gateway-id".into()),
            ]),
            Vec::<String>::new(),
        )))
        .build()
        .unwrap();

    let mut stream = futures_executor::block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: workers.common.model_ref.clone(),
            context: cloudflare_context(Some("You are helpful.")),
            options: SimpleGenerationOptions {
                max_output_tokens: Some(1_234),
                reasoning: Some(ReasoningLevel::High),
                session_id: Some("session-1".into()),
                ..Default::default()
            },
        },
        CancellationToken::new(),
    ))
    .expect("Cloudflare Workers AI stream");
    futures_executor::block_on(async { while stream.next().await.is_some() {} });

    let requests = capture.requests.lock().unwrap();
    let request = requests.first().expect("Workers AI request");
    assert_eq!(
        request.url.as_str(),
        "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat/chat/completions"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("Workers AI request JSON");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["max_tokens"], 1_234);
    for field in [
        "tools",
        "max_completion_tokens",
        "reasoning_effort",
        "store",
    ] {
        assert!(body.get(field).is_none(), "unexpected {field}");
    }
    assert!(!request.headers.contains_key(http::header::AUTHORIZATION));
    assert_eq!(request.headers["cf-aig-authorization"], "Bearer cf-token");
    for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert_eq!(request.headers[name], "session-1", "{name}");
    }
    drop(requests);

    let mut headers = HeaderMapSpec::new();
    headers.insert("Authorization".into(), Some("Bearer upstream-token".into()));
    let result = futures_executor::block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: byok.common.model_ref,
            context: cloudflare_context(None),
            options: SimpleGenerationOptions {
                headers,
                ..Default::default()
            },
        },
        CancellationToken::new(),
    ));
    assert!(
        result.is_err(),
        "BYOK capture intentionally returns HTTP 400"
    );
    let requests = capture.requests.lock().unwrap();
    let request = requests.last().expect("BYOK request");
    assert_eq!(
        request.headers[http::header::AUTHORIZATION],
        "Bearer upstream-token"
    );
    assert_eq!(request.headers["cf-aig-authorization"], "Bearer cf-token");
}

#[test]
fn cloudflare_auth_login_and_precedence_pi_exact() {
    // Pi basis: models.ts applies `{ ...stored.env, ...overrides.env }`, so
    // explicit request environment wins the same field before ambient environment;
    // login collects all three fields verbatim, and JavaScript-truthy
    // whitespace-only values remain valid.
    let registration = provider(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let store = Arc::new(InMemoryCredentialStore::new());
    futures_executor::block_on(async {
        let mut lease = store
            .acquire_lease(
                ProviderId::new("cloudflare-ai-gateway"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        lease.replace(Some(Credential::ApiKey(ApiKeyCredential {
            key: Some(SecretString::new("stored-key")),
            environment: BTreeMap::from([(
                "CLOUDFLARE_ACCOUNT_ID".into(),
                "stored-account".into(),
            )]),
        })));
        lease.commit().await.unwrap();
    });
    let mut request = ResolveAuthRequest::isolated(
        registration.descriptor.clone(),
        Some(registration.catalog.snapshot()[0].clone()),
    );
    request.credential_store = store;
    request.auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([
            ("CLOUDFLARE_API_KEY".into(), "ambient-key".into()),
            ("CLOUDFLARE_ACCOUNT_ID".into(), "ambient-account".into()),
            ("CLOUDFLARE_GATEWAY_ID".into(), "ambient-gateway".into()),
        ]),
        [],
    ));
    request.overrides.environment = BTreeMap::from([
        ("CLOUDFLARE_ACCOUNT_ID".into(), "request-account".into()),
        ("CLOUDFLARE_GATEWAY_ID".into(), "request-gateway".into()),
    ]);
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://gateway.ai.cloudflare.com/v1/request-account/request-gateway/anthropic"
    );
    assert_eq!(
        resolved.headers["cf-aig-authorization"],
        "Bearer stored-key"
    );

    let interaction = Arc::new(RecordingInteraction::with_answers([
        AuthAnswer::Text("login-key".into()),
        AuthAnswer::Text("login-account".into()),
        AuthAnswer::Text("login-gateway".into()),
    ]));
    let credential = futures_executor::block_on(
        registration
            .auth
            .login(interaction, CancellationToken::new()),
    )
    .unwrap();
    let Credential::ApiKey(credential) = credential else {
        panic!("Cloudflare login credential")
    };
    assert_eq!(credential.key.unwrap().expose_secret(), "login-key");
    assert_eq!(
        credential.environment["CLOUDFLARE_ACCOUNT_ID"],
        "login-account"
    );
    assert_eq!(
        credential.environment["CLOUDFLARE_GATEWAY_ID"],
        "login-gateway"
    );

    let credential = futures_executor::block_on(registration.auth.login(
        Arc::new(RecordingInteraction::with_answers([
            AuthAnswer::Text("   ".into()),
            AuthAnswer::Text("\t".into()),
            AuthAnswer::Text(" \t ".into()),
        ])),
        CancellationToken::new(),
    ))
    .unwrap();
    let Credential::ApiKey(credential) = credential else {
        panic!("Cloudflare whitespace login credential")
    };
    assert_eq!(credential.key.unwrap().expose_secret(), "   ");
    assert_eq!(credential.environment["CLOUDFLARE_ACCOUNT_ID"], "\t");
    assert_eq!(credential.environment["CLOUDFLARE_GATEWAY_ID"], " \t ");
}

#[derive(Default)]
struct CapturingBinding {
    runs: Mutex<Vec<(String, GatewayBindingRequest)>>,
    cancellations: Mutex<Vec<CancellationToken>>,
}

impl GatewayBinding for CapturingBinding {
    fn run(
        &self,
        gateway: String,
        request: GatewayBindingRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.runs.lock().unwrap().push((gateway, request));
        self.cancellations.lock().unwrap().push(cancellation);
        Box::pin(async {
            let mut headers = http::HeaderMap::new();
            headers.insert("cf-aig-log-id", "log-1".parse().unwrap());
            Ok(HttpResponse::from_bytes(
                207,
                headers,
                b"data: {}\n\n".to_vec(),
            ))
        })
    }
}

fn binding_request(url: &str, method: http::Method, body: &[u8]) -> HttpRequest {
    let mut headers = http::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert("Content-Length", "17".parse().unwrap());
    headers.insert(
        "CF-AIG-Authorization",
        format!("Bearer {CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}")
            .parse()
            .unwrap(),
    );
    headers.insert("cf-aig-metadata", r#"{"user":"42"}"#.parse().unwrap());
    headers.insert("x-api-key", "provider-key".parse().unwrap());
    HttpRequest {
        method,
        url: Url::parse(url).unwrap(),
        headers,
        auth_headers: http::HeaderMap::new(),
        session_id: None,
        body: body.to_vec(),
        timeout: Some(Duration::from_secs(1)),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    }
}

#[test]
fn cloudflare_gateway_binding_pi_exact() {
    // Pi basis: packages/ai/test/cloudflare-gateway-binding.test.ts and
    // api/cloudflare-gateway-binding.ts. Rust's HttpRequest already represents
    // Fetch's resolved input/init request, so this exercises the structural
    // universal-endpoint contract after that boundary.
    let binding = Arc::new(CapturingBinding::default());
    let transport = GatewayBindingTransport::new(
        binding.clone(),
        Url::parse("https://gateway.ai.cloudflare.com/v1/account-id/my-gateway").unwrap(),
        "my-gateway",
    );
    for (url, body) in [
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/v1/messages",
            br#"{"model":"claude"}"#.as_slice(),
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses?beta=true",
            br#"{"model":"gpt"}"#.as_slice(),
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/workers-ai/v1/chat/completions",
            br#"{"model":"@cf/meta/llama"}"#.as_slice(),
        ),
    ] {
        futures_executor::block_on(transport.execute(
            binding_request(url, http::Method::POST, body),
            CancellationToken::new(),
        ))
        .expect("binding provider/endpoint translation");
    }
    let forwarded_cancellation = CancellationToken::new();
    let mut response = futures_executor::block_on(transport.execute(
        binding_request(
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses?beta=true",
            http::Method::POST,
            br#"{"model":"gpt"}"#,
        ),
        forwarded_cancellation.clone(),
    ))
    .unwrap();
    assert_eq!(response.status, 207);
    assert_eq!(response.headers["cf-aig-log-id"], "log-1");
    assert_eq!(
        futures_executor::block_on(response.body.next())
            .expect("streaming response chunk")
            .expect("streaming response bytes"),
        b"data: {}\n\n"
    );
    let runs = binding.runs.lock().unwrap();
    assert_eq!(
        runs.iter()
            .take(3)
            .map(|(_, request)| (request.provider.as_str(), request.endpoint.as_str()))
            .collect::<Vec<_>>(),
        [
            ("anthropic", "v1/messages"),
            ("openai", "responses?beta=true"),
            ("workers-ai", "v1/chat/completions"),
        ]
    );
    let (gateway, request) = &runs[3];
    assert_eq!(gateway, "my-gateway");
    assert_eq!(request.provider, "openai");
    assert_eq!(request.endpoint, "responses?beta=true");
    assert_eq!(request.query, serde_json::json!({"model":"gpt"}));
    assert!(!request.headers.contains_key("cf-aig-authorization"));
    assert!(!request.headers.contains_key("content-length"));
    assert_eq!(request.headers["cf-aig-metadata"], r#"{"user":"42"}"#);
    assert_eq!(request.headers["x-api-key"], "provider-key");
    drop(runs);
    forwarded_cancellation.cancel();
    assert!(
        binding
            .cancellations
            .lock()
            .unwrap()
            .last()
            .is_some_and(CancellationToken::is_cancelled)
    );

    for (url, method, body, needle) in [
        (
            "https://api.openai.com/v1/responses",
            http::Method::POST,
            b"{}".as_slice(),
            "outside the configured gateway prefix",
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/other-account/my-gateway/openai/responses",
            http::Method::POST,
            b"{}".as_slice(),
            "outside the configured gateway prefix",
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            http::Method::GET,
            b"{}".as_slice(),
            "only POST",
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/openai/responses",
            http::Method::POST,
            b"not json".as_slice(),
            "non-JSON body",
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic",
            http::Method::POST,
            b"{}".as_slice(),
            "missing provider/endpoint path",
        ),
        (
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/../other-gateway/anthropic/v1/messages",
            http::Method::POST,
            b"{}".as_slice(),
            "outside the configured gateway prefix",
        ),
    ] {
        let error = futures_executor::block_on(
            transport.execute(binding_request(url, method, body), CancellationToken::new()),
        )
        .unwrap_err();
        assert!(error.to_string().contains(needle));
    }

    futures_executor::block_on(transport.execute(
        binding_request(
            "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway/anthropic/../anthropic/v1/./messages",
            http::Method::POST,
            br#"{"model":"claude"}"#,
        ),
        CancellationToken::new(),
    ))
    .expect("URL-normalized dot segments route like their normal form");
    let runs = binding.runs.lock().unwrap();
    let (_, normalized) = runs.last().expect("normalized binding run");
    assert_eq!(normalized.provider, "anthropic");
    assert_eq!(normalized.endpoint, "v1/messages");
}
