//! Azure OpenAI provider-owned configuration conformance.

use agentprism_ai::*;
use agentprism_azure_openai_responses::*;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

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

fn ambient() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("AZURE_OPENAI_API_KEY".into(), "azure-secret".into()),
        (
            "AZURE_OPENAI_RESOURCE_NAME".into(),
            "ambient-resource".into(),
        ),
        ("AZURE_OPENAI_API_VERSION".into(), "2026-06-01".into()),
        (
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(),
            r#"{"gpt-5":"production-gpt-5"}"#.into(),
        ),
    ])
}

fn seed_send_azure_credential(
    store: &Arc<InMemoryCredentialStore>,
    environment: BTreeMap<String, String>,
) {
    futures_executor::block_on(async {
        let mut lease = store
            .acquire_lease(
                ProviderId::new("azure-openai-responses"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        lease.replace(Some(Credential::ApiKey(ApiKeyCredential {
            key: Some(SecretString::new("stored-azure-secret")),
            environment,
        })));
        lease.commit().await.unwrap();
    });
}

fn seed_local_azure_credential(
    store: &Rc<LocalInMemoryCredentialStore>,
    environment: BTreeMap<String, String>,
) {
    futures_executor::block_on(async {
        let mut lease = store
            .acquire_lease(
                ProviderId::new("azure-openai-responses"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        lease.replace(Some(Credential::ApiKey(ApiKeyCredential {
            key: Some(SecretString::new("stored-azure-secret")),
            environment,
        })));
        lease.commit().await.unwrap();
    });
}

#[test]
fn azure_openai_auth_options_and_environment_pi_exact_send_and_local() {
    // Pi basis: api/azure-openai-responses.ts:42-49 and 221-232. Explicit
    // azureBaseUrl/azureResourceName and the API-version/deployment-map
    // environment values are provider-owned inputs to endpoint construction.
    let registration = provider(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(resolved.headers["api-key"], "azure-secret");
    assert!(resolved.api_key.is_none());
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(
        resolved.transport_headers[agentprism_openai::AZURE_API_VERSION_AUTH_HEADER],
        "2026-06-01"
    );
    assert_eq!(
        resolved.transport_headers[agentprism_openai::AZURE_DEPLOYMENT_MAP_AUTH_HEADER],
        r#"{"gpt-5":"production-gpt-5"}"#
    );

    let mut request = ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request.overrides.environment.insert(
        "AZURE_OPENAI_BASE_URL".into(),
        "https://explicit-resource.openai.azure.com".into(),
    );
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://explicit-resource.openai.azure.com/openai/v1"
    );

    let registration = local_provider(LocalProviderInputs {
        http: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();
    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(resolved.headers["api-key"], "azure-secret");
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(
        resolved.transport_headers[agentprism_openai::AZURE_API_VERSION_AUTH_HEADER],
        "2026-06-01"
    );
}

#[test]
fn azure_openai_whitespace_configuration_precedence_send_and_local() {
    // Pi basis: api/azure-openai-responses.ts `getAzureConfig` and
    // `getProviderEnvValue`. A scoped whitespace-only base URL wins the
    // environment lookup, but Pi trims it before base-URL truthiness and then
    // falls through to the resource/model URL. Resource names are interpolated
    // unchanged, and API versions retain JavaScript truthiness semantics.
    let registration = provider(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_BASE_URL".into(), "   ".into());
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_RESOURCE_NAME".into(), " scoped ".into());
    let error =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect_err("resource name must not be trimmed before interpolation");
    assert_eq!(error.code(), "invalid_azure_openai_base_url");

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_API_VERSION".into(), "   ".into());
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.transport_headers[agentprism_openai::AZURE_API_VERSION_AUTH_HEADER],
        "   "
    );

    let store = Arc::new(InMemoryCredentialStore::default());
    seed_send_azure_credential(
        &store,
        BTreeMap::from([("AZURE_OPENAI_BASE_URL".into(), "   ".into())]),
    );
    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.credential_store = store;
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );

    let registration = local_provider(LocalProviderInputs {
        http: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();

    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_BASE_URL".into(), "   ".into());
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );

    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_RESOURCE_NAME".into(), " scoped ".into());
    let error =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect_err("local resource name must not be trimmed before interpolation");
    assert_eq!(error.code(), "invalid_azure_openai_base_url");

    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    request
        .overrides
        .environment
        .insert("AZURE_OPENAI_API_VERSION".into(), "   ".into());
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.transport_headers[agentprism_openai::AZURE_API_VERSION_AUTH_HEADER],
        "   "
    );

    let store = Rc::new(LocalInMemoryCredentialStore::default());
    seed_local_azure_credential(
        &store,
        BTreeMap::from([("AZURE_OPENAI_BASE_URL".into(), "   ".into())]),
    );
    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model));
    request.credential_store = store;
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://ambient-resource.openai.azure.com/openai/v1"
    );
}

#[test]
fn azure_openai_base_url_ecmascript_trim_send_and_local() {
    // Architecture v2 part 2 §9.2 and §10.8; Pi basis:
    // api/azure-openai-responses.ts `normalizeAzureBaseUrl` and
    // `resolveAzureConfig`. JavaScript trim removes U+FEFF but not U+0085.
    let registration = provider(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request.overrides.environment.insert(
        "AZURE_OPENAI_BASE_URL".into(),
        "\u{feff}https://feff-resource.openai.azure.com\u{feff}".into(),
    );
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://feff-resource.openai.azure.com/openai/v1"
    );

    let mut request =
        ResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Arc::new(MapAuthContext::new(ambient(), []));
    request.overrides.environment.insert(
        "AZURE_OPENAI_BASE_URL".into(),
        "\u{0085}https://u0085-resource.openai.azure.com\u{0085}".into(),
    );
    let error =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect_err("Send U+0085 base URL must remain untrimmed");
    assert_eq!(error.code(), "invalid_azure_openai_base_url");

    let registration = local_provider(LocalProviderInputs {
        http: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model = registration.catalog.snapshot()[0].clone();

    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model.clone()));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    request.overrides.environment.insert(
        "AZURE_OPENAI_BASE_URL".into(),
        "\u{feff}https://feff-resource.openai.azure.com\u{feff}".into(),
    );
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .unwrap()
            .unwrap();
    assert_eq!(
        resolved.base_url.unwrap().as_str(),
        "https://feff-resource.openai.azure.com/openai/v1"
    );

    let mut request =
        LocalResolveAuthRequest::isolated(registration.descriptor.clone(), Some(model));
    request.auth_context = Rc::new(MapAuthContext::new(ambient(), []));
    request.overrides.environment.insert(
        "AZURE_OPENAI_BASE_URL".into(),
        "\u{0085}https://u0085-resource.openai.azure.com\u{0085}".into(),
    );
    let error =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect_err("Local U+0085 base URL must remain untrimmed");
    assert_eq!(error.code(), "invalid_azure_openai_base_url");
}

#[derive(Clone, Default)]
struct CaptureAzureBodies {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    urls: Arc<Mutex<Vec<url::Url>>>,
}

impl CaptureAzureBodies {
    fn push(&self, url: url::Url, body: Vec<u8>) {
        self.bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body);
        self.urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(url);
    }

    fn last_model(&self) -> String {
        let bodies = self
            .bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        serde_json::from_slice::<serde_json::Value>(bodies.last().expect("Azure request body"))
            .expect("Azure request JSON")["model"]
            .as_str()
            .expect("Azure deployment model")
            .to_owned()
    }

    fn last_api_version(&self) -> String {
        self.urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .expect("Azure request URL")
            .query_pairs()
            .find_map(|(name, value)| (name == "api-version").then(|| value.into_owned()))
            .expect("Azure api-version query parameter")
    }
}

impl HttpTransport for CaptureAzureBodies {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.push(request.url, request.body);
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                400,
                http::HeaderMap::new(),
                b"rejected after capture".to_vec(),
            ))
        })
    }
}

impl LocalHttpTransport for CaptureAzureBodies {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.push(request.url, request.body);
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                400,
                http::HeaderMap::new(),
                b"rejected after capture".to_vec(),
            ))
        })
    }
}

fn azure_context() -> Context {
    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("azure-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("azure-user-text"),
            text: "Hello".into(),
        }],
        timestamp: Timestamp::default(),
    }));
    context
}

fn azure_request(model: ModelDescriptor, explicit_deployment: &str) -> ResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    let model_id = model.common.model_ref.model.to_string();
    let mut auth_headers = http::HeaderMap::new();
    auth_headers.insert(
        agentprism_openai::AZURE_DEPLOYMENT_MAP_AUTH_HEADER,
        http::HeaderValue::from_str(&format!("{model_id}=first,{model_id}=last=discarded"))
            .unwrap(),
    );
    let options = SimpleGenerationOptions {
        api_options: Some(ErasedApiOptionsPatch {
            api: ApiId::new("azure-openai-responses"),
            schema_version: 1,
            value: serde_json::value::RawValue::from_string(format!(
                "{{\"azureDeploymentName\":{}}}",
                serde_json::to_string(explicit_deployment).unwrap()
            ))
            .unwrap(),
        }),
        ..Default::default()
    };
    ResolvedApiRequest {
        model,
        context: azure_context(),
        request_options: ApiRequestOptions::from(&options),
        options,
        full_options: None,
        endpoint,
        headers: http::HeaderMap::new(),
        auth_headers,
        api_key: None,
        api: ApiId::new("azure-openai-responses"),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_azure_request(
    model: ModelDescriptor,
    explicit_deployment: &str,
) -> LocalResolvedApiRequest {
    let request = azure_request(model, explicit_deployment);
    LocalResolvedApiRequest {
        model: request.model,
        context: request.context,
        options: request.options,
        full_options: request.full_options,
        request_options: request.request_options,
        endpoint: request.endpoint,
        headers: request.headers,
        auth_headers: request.auth_headers,
        api_key: request.api_key,
        api: request.api,
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: request.retry_policy,
        timeout: request.timeout,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

#[test]
fn azure_deployment_map_truthiness_send_and_local() {
    // Architecture v2 part 2 §9.2 and §10.8; Pi basis:
    // api/azure-openai-responses.ts `parseDeploymentNameMap` and
    // `resolveDeploymentName`.
    assert_eq!(
        agentprism_openai::parse_azure_deployment_name_map(
            "model=first,model=second=discarded,invalid,=missing"
        ),
        vec![("model".into(), "second".into())]
    );

    let model = models().unwrap().remove(0);
    let send = CaptureAzureBodies::default();
    let api = agentprism_openai::azure_openai_responses_api(Arc::new(send.clone()));
    let result = futures_executor::block_on(
        api.stream(azure_request(model.clone(), ""), CancellationToken::new()),
    );
    assert!(result.is_err(), "capture response intentionally rejects");
    assert_eq!(send.last_model(), "last");

    let local = CaptureAzureBodies::default();
    let api = agentprism_openai::local_azure_openai_responses_api(Rc::new(local.clone()));
    let result = futures_executor::block_on(
        api.stream(local_azure_request(model, ""), CancellationToken::new()),
    );
    assert!(result.is_err(), "capture response intentionally rejects");
    assert_eq!(local.last_model(), "last");
}

#[test]
fn azure_deployment_map_ecmascript_trim_send_and_local() {
    // Architecture v2 part 2 §9.2 and §10.8; Pi basis:
    // api/azure-openai-responses.ts `parseDeploymentNameMap` and
    // `resolveDeploymentName`. JavaScript trim removes U+FEFF but not U+0085.
    let model = models().unwrap().remove(0);
    let model_id = model.common.model_ref.model.to_string();

    for (padding, expected) in [
        ("\u{feff}", "feff-deployment"),
        ("\u{0085}", model_id.as_str()),
    ] {
        let map = format!("{padding}{model_id}{padding}={padding}feff-deployment{padding}");

        let mut send_request = azure_request(model.clone(), "");
        send_request.auth_headers.insert(
            agentprism_openai::AZURE_DEPLOYMENT_MAP_AUTH_HEADER,
            http::HeaderValue::from_bytes(map.as_bytes()).unwrap(),
        );
        let send = CaptureAzureBodies::default();
        let api = agentprism_openai::azure_openai_responses_api(Arc::new(send.clone()));
        let result = futures_executor::block_on(api.stream(send_request, CancellationToken::new()));
        assert!(result.is_err(), "capture response intentionally rejects");
        assert_eq!(send.last_model(), expected);

        let mut local_request = local_azure_request(model.clone(), "");
        local_request.auth_headers.insert(
            agentprism_openai::AZURE_DEPLOYMENT_MAP_AUTH_HEADER,
            http::HeaderValue::from_bytes(map.as_bytes()).unwrap(),
        );
        let local = CaptureAzureBodies::default();
        let api = agentprism_openai::local_azure_openai_responses_api(Rc::new(local.clone()));
        let result =
            futures_executor::block_on(api.stream(local_request, CancellationToken::new()));
        assert!(result.is_err(), "capture response intentionally rejects");
        assert_eq!(local.last_model(), expected);
    }
}

#[test]
fn azure_api_version_whitespace_truthiness_send_and_local() {
    // Architecture v2 part 2 §9.2 and §10.8; Pi basis:
    // api/azure-openai-responses.ts `getAzureConfig`. A credential-carried
    // non-empty API version is selected by JavaScript truthiness and retained
    // byte-for-byte, even when it consists only of whitespace.
    let model = models().unwrap().remove(0);
    let mut send_request = azure_request(model.clone(), "deployment");
    send_request.auth_headers.insert(
        agentprism_openai::AZURE_API_VERSION_AUTH_HEADER,
        http::HeaderValue::from_static("   "),
    );
    let send = CaptureAzureBodies::default();
    let api = agentprism_openai::azure_openai_responses_api(Arc::new(send.clone()));
    let result = futures_executor::block_on(api.stream(send_request, CancellationToken::new()));
    assert!(result.is_err(), "capture response intentionally rejects");
    assert_eq!(send.last_api_version(), "   ");

    let mut local_request = local_azure_request(model, "deployment");
    local_request.auth_headers.insert(
        agentprism_openai::AZURE_API_VERSION_AUTH_HEADER,
        http::HeaderValue::from_static("   "),
    );
    let local = CaptureAzureBodies::default();
    let api = agentprism_openai::local_azure_openai_responses_api(Rc::new(local.clone()));
    let result = futures_executor::block_on(api.stream(local_request, CancellationToken::new()));
    assert!(result.is_err(), "capture response intentionally rejects");
    assert_eq!(local.last_api_version(), "   ");
}
