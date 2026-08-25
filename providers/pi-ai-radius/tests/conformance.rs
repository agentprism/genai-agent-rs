//! Radius dynamic-catalog conformance.

use http::{HeaderMap, header};
use pi_ai::*;
use pi_ai_radius::*;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

#[path = "../../fixtures/oauth_support.rs"]
mod support;
use support::*;

const CONFIG: &str = r#"{"baseUrl":"https://radius.pi.dev/v1","models":[{"id":"auto","name":"Radius Auto","reasoning":true,"thinkingLevelMap":{"off":null,"high":"high"},"input":["text","image"],"cost":{"input":1.25,"output":2.5,"cacheRead":0.125,"cacheWrite":0.25},"contextWindow":128000,"maxTokens":16384}]}"#;

#[derive(Clone)]
struct ConfigTransport {
    expected_url: &'static str,
}

impl HttpTransport for ConfigTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        assert_eq!(request.method, http::Method::GET);
        assert_eq!(request.url.as_str(), self.expected_url);
        assert_eq!(
            request.headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-token"
        );
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                CONFIG.into(),
            ))
        })
    }
}

impl LocalHttpTransport for ConfigTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        assert_eq!(request.method, http::Method::GET);
        assert_eq!(request.url.as_str(), self.expected_url);
        assert_eq!(
            request.headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-token"
        );
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                CONFIG.into(),
            ))
        })
    }
}

fn fetch_context() -> CatalogFetchContext {
    fetch_context_with_gateway(None)
}

fn fetch_context_with_gateway(base_url: Option<Url>) -> CatalogFetchContext {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer test-token".parse().unwrap());
    CatalogFetchContext {
        provider: ProviderId::new("radius"),
        stored: None,
        auth: ResolvedAuth {
            api_key: None,
            headers,
            transport_headers: HeaderMap::new(),
            base_url,
            source: AuthSource::new("test"),
        },
        force: false,
    }
}

#[test]
fn radius_dynamic_catalog_source_pi_exact_send_and_local() {
    // Architecture §5.7; Pi basis: providers/radius.ts refreshModels and
    // providers/radius-config.ts loadRadiusGatewayConfig/getRadiusModelsFromConfig.
    let send = RadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Arc::new(ConfigTransport {
            expected_url: "https://radius.pi.dev/v1/config",
        }),
    );
    let local = LocalRadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Rc::new(ConfigTransport {
            expected_url: "https://radius.pi.dev/v1/config",
        }),
    );
    let send_candidate = futures_executor::block_on(ModelCatalogSource::fetch(
        &send,
        fetch_context(),
        CancellationToken::new(),
    ))
    .unwrap();
    let local_candidate = futures_executor::block_on(LocalModelCatalogSource::fetch(
        &local,
        fetch_context(),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(send_candidate.models, local_candidate.models);
    let model = &send_candidate.models[0];
    assert_eq!(model.common.model_ref, ModelRef::new("radius", "auto"));
    assert_eq!(model.api.api_id(), ApiId::new("pi-messages"));
    assert_eq!(
        model.common.pricing.default.input,
        MoneyRate::new(1_250_000)
    );
}

#[derive(Clone)]
struct StaticConfigTransport {
    status: u16,
    body: String,
}

impl HttpTransport for StaticConfigTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let status = self.status;
        let body = self.body.clone().into_bytes();
        Box::pin(async move { Ok(HttpResponse::from_bytes(status, HeaderMap::new(), body)) })
    }
}

impl LocalHttpTransport for StaticConfigTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let status = self.status;
        let body = self.body.clone().into_bytes();
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                status,
                HeaderMap::new(),
                body,
            ))
        })
    }
}

#[test]
fn radius_config_sanitizer_retains_valid_entries_pi_exact_send_and_local() {
    // Architecture part 2 §5.7; Pi basis: providers/radius-config.ts:26-49,
    // where malformed model entries are filtered independently.
    let body = format!(
        r#"{{"baseUrl":"https://radius.pi.dev/v1","models":[null,{{"id":"missing-fields"}},{valid},"bad"]}}"#,
        valid = &CONFIG[CONFIG.find("{\"id\"").unwrap()..CONFIG.len() - 2]
    );
    let transport = StaticConfigTransport { status: 200, body };
    let send = RadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Arc::new(transport.clone()),
    );
    let local = LocalRadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Rc::new(transport),
    );
    let send_candidate = futures_executor::block_on(ModelCatalogSource::fetch(
        &send,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect("Send sanitized Radius candidate");
    let local_candidate = futures_executor::block_on(LocalModelCatalogSource::fetch(
        &local,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect("Local sanitized Radius candidate");
    assert_eq!(send_candidate.models, local_candidate.models);
    assert_eq!(send_candidate.models.len(), 1);
    assert_eq!(
        send_candidate.models[0].common.model_ref,
        ModelRef::new("radius", "auto")
    );
}

#[test]
fn radius_non_success_detail_is_bounded_pi_exact_send_and_local() {
    // Architecture part 2 §5.7; Pi basis: providers/radius-config.ts:75-95.
    let body = format!("  {}tail  \n", "x".repeat(520));
    let expected = format!(
        "Could not load Radius config from https://radius.pi.dev: 502: {}…",
        "x".repeat(512)
    );
    let transport = StaticConfigTransport { status: 502, body };
    let send = RadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Arc::new(transport.clone()),
    );
    let local = LocalRadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Rc::new(transport),
    );
    let send_error = futures_executor::block_on(ModelCatalogSource::fetch(
        &send,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect_err("Send non-success Radius response");
    let local_error = futures_executor::block_on(LocalModelCatalogSource::fetch(
        &local,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect_err("Local non-success Radius response");
    assert_eq!(send_error.message, expected);
    assert_eq!(local_error.message, expected);
}

#[test]
fn radius_non_success_detail_uses_ecmascript_trim_pi_exact_send_and_local() {
    // Architecture part 2 §5.7 and §9.2; Pi basis:
    // packages/ai/src/providers/radius-config.ts `truncateHttpBody`, whose
    // JavaScript `trim()` includes U+FEFF but excludes U+0085.
    let body = "\u{feff}detail\u{0085}\u{feff}".into();
    let expected = "Could not load Radius config from https://radius.pi.dev: 502: detail\u{0085}";
    let transport = StaticConfigTransport { status: 502, body };
    let send = RadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Arc::new(transport.clone()),
    );
    let local = LocalRadiusCatalogSource::new(
        "radius",
        Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
        Rc::new(transport),
    );
    let send_error = futures_executor::block_on(ModelCatalogSource::fetch(
        &send,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect_err("Send non-success Radius ECMAScript-trim response");
    let local_error = futures_executor::block_on(LocalModelCatalogSource::fetch(
        &local,
        fetch_context(),
        CancellationToken::new(),
    ))
    .expect_err("Local non-success Radius ECMAScript-trim response");
    assert_eq!(send_error.message, expected);
    assert_eq!(local_error.message, expected);
}

#[test]
fn radius_catalog_uses_credential_gateway_send_and_local() {
    // Architecture part 2 §5.7; Pi basis: auth/oauth/radius.ts `toAuth`
    // supplies the selected credential gateway to providers/radius.ts before
    // `loadRadiusGatewayConfig` resolves `/v1/config`.
    let configured_gateway = Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap();
    let credential_gateway = Url::parse("https://credential.radius.test/custom").unwrap();
    let send = RadiusCatalogSource::new(
        "radius",
        configured_gateway.clone(),
        Arc::new(ConfigTransport {
            expected_url: "https://credential.radius.test/v1/config",
        }),
    );
    let local = LocalRadiusCatalogSource::new(
        "radius",
        configured_gateway,
        Rc::new(ConfigTransport {
            expected_url: "https://credential.radius.test/v1/config",
        }),
    );

    let send_candidate = futures_executor::block_on(ModelCatalogSource::fetch(
        &send,
        fetch_context_with_gateway(Some(credential_gateway.clone())),
        CancellationToken::new(),
    ))
    .unwrap();
    let local_candidate = futures_executor::block_on(LocalModelCatalogSource::fetch(
        &local,
        fetch_context_with_gateway(Some(credential_gateway)),
        CancellationToken::new(),
    ))
    .unwrap();

    assert_eq!(send_candidate.models, local_candidate.models);
}

#[test]
fn radius_provider_uses_dynamic_catalog_and_pi_messages_send_and_local() {
    // Architecture §5.7; Pi basis: providers/radius.ts radiusProvider.
    let send = radius_provider(Arc::new(ConfigTransport {
        expected_url: "https://radius.pi.dev/v1/config",
    }))
    .unwrap();
    let local = local_radius_provider(Rc::new(ConfigTransport {
        expected_url: "https://radius.pi.dev/v1/config",
    }))
    .unwrap();
    assert!(send.catalog.catalog_source().is_some());
    assert!(local.catalog.catalog_source().is_some());
    assert!(send.apis.contains_key(&ApiId::new("pi-messages")));
    assert!(local.apis.contains_key(&ApiId::new("pi-messages")));
}

#[derive(Clone)]
struct RejectingEnvironmentAuthContext {
    environment_accesses: Arc<AtomicUsize>,
}

impl AuthContext for RejectingEnvironmentAuthContext {
    fn env(
        &self,
        name: String,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<String>, AuthError>> {
        self.environment_accesses.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(AuthError::new(
                "unexpected_environment_access",
                format!("unexpected environment access: {name}"),
            ))
        })
    }

    fn file_exists(
        &self,
        _path: String,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<bool, AuthError>> {
        Box::pin(async { Ok(false) })
    }
}

impl LocalAuthContext for RejectingEnvironmentAuthContext {
    fn env(
        &self,
        name: String,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<String>, AuthError>> {
        self.environment_accesses.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(AuthError::new(
                "unexpected_environment_access",
                format!("unexpected environment access: {name}"),
            ))
        })
    }

    fn file_exists(
        &self,
        _path: String,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<bool, AuthError>> {
        Box::pin(async { Ok(false) })
    }
}

fn radius_stored_oauth() -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new("stored-access"),
        refresh: SecretString::new("stored-refresh"),
        expires_at: Timestamp::default(),
        extra: ProviderOAuthExtra::Radius {
            gateway_url: Url::parse(DEFAULT_RADIUS_GATEWAY).unwrap(),
            organization_id: None,
        },
    })
}

fn seed_radius_send(store: &Arc<InMemoryCredentialStore>) {
    futures_executor::block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new("radius"), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(radius_stored_oauth()));
        lease.commit().await.unwrap();
    });
}

fn seed_radius_local(store: &Rc<LocalInMemoryCredentialStore>) {
    futures_executor::block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new("radius"), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(radius_stored_oauth()));
        lease.commit().await.unwrap();
    });
}

#[test]
fn radius_configuration_check_skips_cache_retention_environment_send_and_local_pi_exact() {
    // Architecture part 2 §9.2 and §10.7; Pi basis:
    // packages/ai/src/models.ts `checkProviderAuth`, where a stored OAuth
    // credential is configuration-complete without provider-specific request
    // decoration or environment lookup.
    let send_accesses = Arc::new(AtomicUsize::new(0));
    let send_store = Arc::new(InMemoryCredentialStore::default());
    seed_radius_send(&send_store);
    let send_models = Models::builder()
        .credential_store(send_store)
        .auth_context(Arc::new(RejectingEnvironmentAuthContext {
            environment_accesses: Arc::clone(&send_accesses),
        }))
        .provider(
            radius_provider(Arc::new(StaticConfigTransport {
                status: 200,
                body: CONFIG.into(),
            }))
            .unwrap(),
        )
        .build()
        .unwrap();
    let send_check = futures_executor::block_on(
        send_models.check_auth(ProviderId::new("radius"), CancellationToken::new()),
    )
    .expect("Send Radius configuration check")
    .expect("stored Send Radius OAuth");
    assert_eq!(send_check.credential_type, CredentialType::OAuth);
    assert_eq!(send_accesses.load(Ordering::SeqCst), 0);

    let local_accesses = Arc::new(AtomicUsize::new(0));
    let local_store = Rc::new(LocalInMemoryCredentialStore::default());
    seed_radius_local(&local_store);
    let local_models = LocalModels::builder()
        .credential_store(local_store)
        .auth_context(Rc::new(RejectingEnvironmentAuthContext {
            environment_accesses: Arc::clone(&local_accesses),
        }))
        .provider(
            local_radius_provider(Rc::new(StaticConfigTransport {
                status: 200,
                body: CONFIG.into(),
            }))
            .unwrap(),
        )
        .build()
        .unwrap();
    let local_check = futures_executor::block_on(
        local_models.check_auth(ProviderId::new("radius"), CancellationToken::new()),
    )
    .expect("Local Radius configuration check")
    .expect("stored Local Radius OAuth");
    assert_eq!(local_check.credential_type, CredentialType::OAuth);
    assert_eq!(local_accesses.load(Ordering::SeqCst), 0);
}

fn radius_oauth_responses() -> [HttpScriptedResponse; 2] {
    [
        HttpScriptedResponse::json(
            200,
            r#"{"device_code":"device","user_code":"RADIUS-1","verification_uri":"https://radius.test/device","expires_in":600,"interval":0}"#,
        ),
        HttpScriptedResponse::json(
            200,
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        ),
    ]
}

#[test]
fn radius_oauth_pi_exact_send_and_local() {
    // Pi basis: packages/ai/test/radius-oauth.test.ts and auth/oauth/radius.ts:
    // user-selected device flow, gateway-relative endpoints, typed gateway
    // metadata, and the concrete Send/Local host contracts.
    let gateway = Url::parse("https://radius.test/").unwrap();
    let send_transport = Arc::new(ScriptedTransport::new(radius_oauth_responses()));
    let interaction = Arc::new(RecordingInteraction::with_answers([AuthAnswer::Selected(
        "device-code".into(),
    )]));
    let oauth = RadiusOAuth::new(send_transport.clone(), gateway.clone());
    let credential =
        futures_executor::block_on(oauth.login(interaction.clone(), CancellationToken::new()))
            .unwrap();
    assert_eq!(credential.access.expose_secret(), "access");
    assert!(matches!(
        credential.extra,
        ProviderOAuthExtra::Radius { ref gateway_url, .. } if gateway_url == &gateway
    ));
    assert!(matches!(
        interaction.notifications.lock().unwrap().as_slice(),
        [AuthEvent::DeviceCode { user_code, .. }] if user_code == "RADIUS-1"
    ));
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(seen[0].url, "https://radius.test/v1/oauth/device");
    assert_eq!(seen[1].url, "https://radius.test/v1/oauth/token");
    drop(seen);

    let local_transport = Rc::new(ScriptedTransport::new(radius_oauth_responses()));
    let local_interaction = Rc::new(RecordingInteraction::with_answers([AuthAnswer::Selected(
        "device-code".into(),
    )]));
    let oauth = LocalRadiusOAuth::new(local_transport.clone(), gateway);
    let credential =
        futures_executor::block_on(oauth.login(local_interaction, CancellationToken::new()))
            .unwrap();
    assert_eq!(credential.refresh.expose_secret(), "refresh");
    assert_eq!(local_transport.seen.lock().unwrap().len(), 2);

    let send_transport = Arc::new(ScriptedTransport::new([HttpScriptedResponse::json(
        200,
        r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
    )]));
    let oauth = RadiusOAuth::new(
        send_transport.clone(),
        Url::parse("https://radius.test").unwrap(),
    );
    let refreshed = futures_executor::block_on(oauth.refresh(
        OAuthCredential {
            access: SecretString::new("old-access"),
            refresh: SecretString::new("old-refresh"),
            expires_at: Timestamp::default(),
            extra: ProviderOAuthExtra::Radius {
                gateway_url: Url::parse("https://radius.test").unwrap(),
                organization_id: None,
            },
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(refreshed.access.expose_secret(), "new-access");
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(seen[0].url, "https://radius.test/v1/oauth/token");
    assert!(String::from_utf8_lossy(&seen[0].body).contains("refresh_token=old-refresh"));
    drop(seen);

    let send_transport = Arc::new(ScriptedTransport::new([HttpScriptedResponse::json(
        200,
        r#"{"issuer":"https://radius-ui.test"}"#,
    )]));
    let oauth = RadiusOAuth::new(
        send_transport.clone(),
        Url::parse("https://radius.test").unwrap(),
    );
    let error = futures_executor::block_on(oauth.login(
        Arc::new(RecordingInteraction::with_answers([AuthAnswer::Selected(
            "browser".into(),
        )])),
        CancellationToken::new(),
    ))
    .unwrap_err();
    assert!(error.to_string().contains("Invalid Radius OAuth config"));
    assert_eq!(
        send_transport.seen.lock().unwrap()[0].url,
        "https://radius.test/v1/oauth"
    );
}
