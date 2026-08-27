use agentprism_ai::{
    ApiId, AssistantImages, AuthError, AuthResolutionOverrides, AuthResolver, AuthSource,
    CacheWriteRetentionPricing, CancellationToken, ErasedPayloadContext, ErasedPayloadTransform,
    HeaderMapSpec, HeaderTransform, HeaderTransformContext, HttpRequest, HttpResponse,
    HttpTransport, ImageCatalogError, ImageGenerationContent, ImageGenerationContext,
    ImageGenerationOptions, ImageGenerationStopReason, ImageHeaderTransformContext, ImageModality,
    ImageModelCatalogSource, ImageModelDescriptor, ImagesApi, LocalAuthResolver, LocalBoxFuture,
    LocalErasedPayloadTransform, LocalHeaderTransform, LocalHttpResponse, LocalHttpTransport,
    LocalImageModelCatalogSource, LocalImagesApi, LocalModels, LocalProviderRegistration,
    LocalResolveAuthRequest, LocalResolvedImageRequest, LocalResponseObserver, MiddlewareError,
    ModelPricing, ModelRef, Models, MoneyRate, OPENROUTER_IMAGES_API_ID,
    PayloadTransformDisposition, ProviderPayload, ProviderRegistration, ProviderResponseMetadata,
    RequestWidePriceTier, ResolveAuthRequest, ResolvedAuth, ResolvedImageRequest,
    ResponseObservationContext, ResponseObserver, SecretString, SendBoxFuture,
    TelemetryContextHandle, TokenPriceRates, TransportError, encode_openrouter_images_request,
};
use http::HeaderMap;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use url::Url;

const CASES: &[&str] = &["text-only", "image-input", "text-and-image-output"];

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn json_response_headers() -> HeaderMap {
    HeaderMap::from_iter([(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    )])
}

fn zero_pricing() -> ModelPricing {
    ModelPricing {
        default: TokenPriceRates {
            input: MoneyRate::new(0),
            output: MoneyRate::new(0),
            cache_read: MoneyRate::new(0),
            cache_write: MoneyRate::new(0),
        },
        request_wide_tiers: Vec::<RequestWidePriceTier>::new(),
        cache_write_retention: CacheWriteRetentionPricing::default(),
    }
}

fn fixture_model(canonical: &Value) -> ImageModelDescriptor {
    let model = canonical.get("model").expect("fixture model");
    let modalities = |field: &str| {
        model[field]
            .as_array()
            .expect("modality array")
            .iter()
            .map(|value| match value.as_str().expect("modality string") {
                "text" => ImageModality::Text,
                "image" => ImageModality::Image,
                other => panic!("unexpected modality {other}"),
            })
            .collect()
    };
    ImageModelDescriptor {
        model_ref: ModelRef::new(
            model["provider"].as_str().expect("provider"),
            model["id"].as_str().expect("model id"),
        ),
        display_name: model["name"].as_str().expect("model name").into(),
        api: ApiId::new(model["api"].as_str().expect("api")),
        base_url: Url::parse("http://127.0.0.1:9/api/v1").expect("fixture URL"),
        input: modalities("input"),
        output: modalities("output"),
        pricing: zero_pricing(),
        headers: HeaderMapSpec::new(),
    }
}

fn fixture_context(canonical: &Value) -> ImageGenerationContext {
    serde_json::from_value(canonical["context"].clone()).expect("canonical image context")
}

fn fixture_file(case: &str, file: &str) -> Vec<u8> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/openrouter-images")
        .join(case)
        .join(file);
    std::fs::read(root).expect("checked fixture file")
}

#[test]
fn wire_openrouter_images_pi_exact() {
    // Pi basis: packages/ai/test/openrouter-images.test.ts and
    // packages/ai/src/api/openrouter-images.ts buildParams.
    for case in CASES {
        let canonical: Value =
            serde_json::from_slice(&fixture_file(case, "canonical.json")).expect("canonical JSON");
        let actual = encode_openrouter_images_request(
            &fixture_model(&canonical),
            &fixture_context(&canonical),
        )
        .expect("request lowering succeeds");
        assert_eq!(
            actual,
            fixture_file(case, "request.body.json"),
            "case {case}"
        );
    }
}

#[test]
fn openrouter_image_catalog_matches_pi_published_data() {
    // Pi basis: packages/ai/test/image-model-data.test.ts and
    // packages/ai/src/image-models.generated.ts at the tracked pin.
    let models = agentprism_openrouter::image_models().expect("published image catalog");
    assert_eq!(models.len(), 45);
    assert_eq!(
        models.first().expect("first").model_ref.model.as_str(),
        "black-forest-labs/flux.2-flex"
    );
    assert_eq!(
        models.last().expect("last").model_ref.model.as_str(),
        "x-ai/grok-imagine-image-quality"
    );
    assert!(
        models
            .iter()
            .all(|model| model.api.as_str() == OPENROUTER_IMAGES_API_ID)
    );
    assert!(
        models
            .iter()
            .all(|model| model.output.contains(&ImageModality::Image))
    );
    let gemini = models
        .iter()
        .find(|model| model.model_ref.model.as_str() == "google/gemini-2.5-flash-image")
        .expect("Pi-published Gemini image model");
    assert_eq!(gemini.input, [ImageModality::Image, ImageModality::Text]);
    assert_eq!(gemini.output, [ImageModality::Image, ImageModality::Text]);
    assert_eq!(gemini.pricing.default.input, MoneyRate::new(300_000));
    assert_eq!(gemini.pricing.default.output, MoneyRate::new(2_500_000));
    assert_eq!(gemini.pricing.default.cache_read, MoneyRate::new(30_000));
    assert_eq!(gemini.pricing.default.cache_write, MoneyRate::new(83_333));
    let auto = models
        .iter()
        .find(|model| model.model_ref.model.as_str() == "openrouter/auto")
        .expect("Pi-published Auto Router image model");
    assert_eq!(
        auto.pricing.default.input,
        MoneyRate::new(-1_000_000_000_000)
    );
    assert_eq!(
        auto.pricing.default.output,
        MoneyRate::new(-1_000_000_000_000)
    );
    let provider =
        agentprism_openrouter::openrouter_provider(Arc::new(FixtureTransport::default()))
            .expect("OpenRouter registration");
    assert_eq!(provider.image_models.as_ref(), models.as_slice());
    assert!(
        provider
            .image_apis
            .contains_key(&ApiId::new(OPENROUTER_IMAGES_API_ID))
    );
}

#[test]
fn openrouter_image_catalog_parser_is_strict() {
    // Pi basis: packages/ai/test/image-model-data.test.ts and
    // packages/ai/scripts/generate-image-models.ts parseOpenRouterImageModels.
    for source in [
        r#"{}"#,
        r#"{"data":[]}"#,
        r#"{"data":null}"#,
        r#"{"data":"invalid"}"#,
    ] {
        let error = agentprism_openrouter::parse_image_models_response(source)
            .expect_err("empty data is rejected");
        assert!(
            error
                .to_string()
                .contains("missing or empty image model list")
        );
    }
    let no_images = r#"{"data":[{"id":"text-only","name":"Text","architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"0.000001"}}]}"#;
    assert!(
        agentprism_openrouter::parse_image_models_response(no_images)
            .expect_err("catalog with no image output is rejected")
            .to_string()
            .contains("no usable image models")
    );

    let raw_catalog = r#"{"data":[{"id":"example/image-model","name":"Example Image Model","architecture":{"input_modalities":["audio","text","image","text"],"output_modalities":["text","image","image"]},"pricing":{"prompt":"0.000001","completion":"0.000002","input_cache_read":"0.00000025","input_cache_write":"5e-7"}}]}"#;
    let parsed = agentprism_openrouter::parse_image_models_response(raw_catalog)
        .expect("raw OpenRouter record parses");
    assert_eq!(parsed.len(), 1);
    let model = &parsed[0];
    assert_eq!(
        model.model_ref,
        ModelRef::new("openrouter", "example/image-model")
    );
    assert_eq!(model.api.as_str(), OPENROUTER_IMAGES_API_ID);
    assert_eq!(model.input, [ImageModality::Text, ImageModality::Image]);
    assert_eq!(model.output, [ImageModality::Text, ImageModality::Image]);
    assert_eq!(model.pricing.default.input, MoneyRate::new(1_000_000));
    assert_eq!(model.pricing.default.output, MoneyRate::new(2_000_000));
    assert_eq!(model.pricing.default.cache_read, MoneyRate::new(250_000));
    assert_eq!(model.pricing.default.cache_write, MoneyRate::new(500_000));

    let default_input = agentprism_openrouter::parse_image_models_response(
        r#"{"data":[{"id":"default-input","name":"Default Input","architecture":{"input_modalities":["audio"],"output_modalities":["image"]}}]}"#,
    )
    .expect("unknown-only input defaults to text");
    assert_eq!(default_input[0].input, [ImageModality::Text]);
}

#[test]
fn openrouter_image_catalog_ignores_incomplete_non_image_records_pi_exact() {
    // Pi basis: packages/ai/scripts/generate-image-models.ts filters records
    // without image output before reading their required id and name fields.
    let parsed = agentprism_openrouter::parse_image_models_response(
        r#"{"data":[{"architecture":{"output_modalities":["text"]}},{"id":"valid/image","name":"Valid Image","architecture":{"input_modalities":["text"],"output_modalities":["image"]}}]}"#,
    )
    .expect("incomplete non-image record is ignored before validation");
    assert_eq!(parsed.len(), 1);
    assert_eq!(
        parsed[0].model_ref,
        ModelRef::new("openrouter", "valid/image")
    );
}

#[test]
fn models_builtin_openrouter_image_provider_catalog_and_auth_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/images-models.test.ts "builtinImagesModels
    // registers the openrouter provider with its catalog" scenario. The
    // explicit key uses the same provider auth path as its ambient
    // OPENROUTER_API_KEY value while keeping this test hermetic.
    let send_transport = Arc::new(FixtureTransport::default());
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(send_transport)
                .expect("send OpenRouter provider"),
        )
        .build()
        .expect("Models");
    assert_eq!(
        send.providers()
            .iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        ["openrouter"]
    );
    assert!(!send.image_models().is_empty());
    assert!(
        send.image_models()
            .iter()
            .all(|model| model.api.as_str() == OPENROUTER_IMAGES_API_ID)
    );
    let send_auth = futures_executor::block_on(send.resolve_auth(
        "openrouter".into(),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("send-image-key")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .expect("send auth resolution")
    .expect("configured send auth");
    assert_eq!(
        send_auth.api_key.as_ref().map(SecretString::expose_secret),
        Some("send-image-key")
    );
    assert_eq!(
        send_auth.headers[http::header::AUTHORIZATION],
        "Bearer send-image-key"
    );

    let local_transport = Rc::new(FixtureTransport::default());
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(local_transport)
                .expect("local OpenRouter provider"),
        )
        .build()
        .expect("LocalModels");
    assert_eq!(
        local
            .providers()
            .iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        ["openrouter"]
    );
    assert!(!local.image_models().is_empty());
    assert!(
        local
            .image_models()
            .iter()
            .all(|model| model.api.as_str() == OPENROUTER_IMAGES_API_ID)
    );
    let local_auth = futures_executor::block_on(local.resolve_auth(
        "openrouter".into(),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("local-image-key")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .expect("local auth resolution")
    .expect("configured local auth");
    assert_eq!(
        local_auth.api_key.as_ref().map(SecretString::expose_secret),
        Some("local-image-key")
    );
    assert_eq!(
        local_auth.headers[http::header::AUTHORIZATION],
        "Bearer local-image-key"
    );
}

#[derive(Default)]
struct FixtureTransport {
    requests: Mutex<Vec<HttpRequest>>,
    responses: Mutex<VecDeque<(u16, Vec<u8>)>>,
}

impl FixtureTransport {
    fn with_response(status: u16, body: Vec<u8>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([(status, body)])),
        }
    }
}

impl HttpTransport for FixtureTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            lock(&self.requests).push(request);
            let (status, body) = lock(&self.responses)
                .pop_front()
                .unwrap_or_else(|| (500, b"unexpected extra request".to_vec()));
            Ok(HttpResponse::from_bytes(
                status,
                json_response_headers(),
                body,
            ))
        })
    }
}

impl LocalHttpTransport for FixtureTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            lock(&self.requests).push(request);
            let (status, body) = lock(&self.responses)
                .pop_front()
                .unwrap_or_else(|| (500, b"unexpected extra request".to_vec()));
            Ok(LocalHttpResponse::from_bytes(
                status,
                json_response_headers(),
                body,
            ))
        })
    }
}

fn configured_models(transport: Arc<FixtureTransport>) -> Models {
    Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(transport)
                .expect("OpenRouter provider registration"),
        )
        .build()
        .expect("Models registration")
}

fn configured_image_model(models: &Models) -> ImageModelDescriptor {
    models
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("published OpenRouter image model")
}

fn explicit_options() -> ImageGenerationOptions {
    let mut auth = AuthResolutionOverrides {
        api_key: Some(SecretString::new("explicit-fixture-key")),
        ..AuthResolutionOverrides::default()
    };
    auth.environment.insert(
        "OPENROUTER_API_KEY".into(),
        "environment-fixture-key".into(),
    );
    ImageGenerationOptions {
        auth,
        ..ImageGenerationOptions::default()
    }
}

fn direct_send_image_request(
    model: ImageModelDescriptor,
    telemetry_context: Option<TelemetryContextHandle>,
) -> ResolvedImageRequest {
    ResolvedImageRequest {
        endpoint: model.base_url.clone(),
        model,
        context: ImageGenerationContext::default(),
        request_options: agentprism_ai::ApiRequestOptions {
            telemetry_context,
            ..agentprism_ai::ApiRequestOptions::default()
        },
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        environment: BTreeMap::new(),
        metadata: BTreeMap::new(),
        api_key: Some(SecretString::new("direct-image-key")),
        payload_transforms: Arc::from([]),
        retry_policy: agentprism_ai::RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(agentprism_ai::DefaultRetryClassifier::default()),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
    }
}

fn direct_local_image_request(
    model: ImageModelDescriptor,
    telemetry_context: Option<TelemetryContextHandle>,
) -> LocalResolvedImageRequest {
    LocalResolvedImageRequest {
        endpoint: model.base_url.clone(),
        model,
        context: ImageGenerationContext::default(),
        request_options: agentprism_ai::ApiRequestOptions {
            telemetry_context,
            ..agentprism_ai::ApiRequestOptions::default()
        },
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        environment: BTreeMap::new(),
        metadata: BTreeMap::new(),
        api_key: Some(SecretString::new("direct-local-image-key")),
        payload_transforms: Rc::from([]),
        retry_policy: agentprism_ai::RetryPolicy::default(),
        timeout: None,
        retry_classifier: Rc::new(agentprism_ai::LocalDefaultRetryClassifier::default()),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
    }
}

#[test]
fn openrouter_images_direct_sdk_authorization_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts createClient passes
    // options.apiKey to OpenAI SDK independently of defaultHeaders. The SDK
    // supplies `Authorization: Bearer <key>` when no logical override exists.
    let response = fixture_file("text-only", "response.body.json");
    let send_transport = Arc::new(FixtureTransport::with_response(200, response.clone()));
    let send_api = agentprism_ai::OpenRouterImagesApi::new(send_transport.clone());
    let send_result = futures_executor::block_on(ImagesApi::generate(
        &send_api,
        direct_send_image_request(synthetic_model("openrouter", "send-sdk-auth"), None),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(
        lock(&send_transport.requests)[0].headers[http::header::AUTHORIZATION],
        "Bearer direct-image-key"
    );

    let local_transport = Rc::new(FixtureTransport::with_response(200, response));
    let local_api = agentprism_ai::LocalOpenRouterImagesApi::new(local_transport.clone());
    let local_result = futures_executor::block_on(LocalImagesApi::generate(
        &local_api,
        direct_local_image_request(synthetic_model("openrouter", "local-sdk-auth"), None),
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(
        lock(&local_transport.requests)[0].headers[http::header::AUTHORIZATION],
        "Bearer direct-local-image-key"
    );
}

#[test]
fn injected_image_http_transport_receives_final_request() {
    // Pi basis: packages/ai/test/fetch-option.test.ts image generation case.
    let body = fixture_file("text-and-image-output", "response.body.json");
    let transport = Arc::new(FixtureTransport::with_response(200, body));
    let models = configured_models(Arc::clone(&transport));
    let model = configured_image_model(&models);
    let result = futures_executor::block_on(models.generate_images(
        model,
        ImageGenerationContext {
            input: vec![ImageGenerationContent::text("draw it")],
        },
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(result.output.len(), 2);
    let requests = lock(&transport.requests);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.path().ends_with("/api/v1/chat/completions"));
    assert_eq!(requests[0].attempt, 0);
    assert_eq!(
        requests[0]
            .headers
            .get(http::header::AUTHORIZATION)
            .expect("authorization")
            .to_str()
            .expect("header text"),
        "Bearer explicit-fixture-key"
    );
}

struct ImageMiddlewareProbe {
    header_calls: Arc<AtomicUsize>,
    payload_calls: Arc<AtomicUsize>,
}

impl HeaderTransform for ImageMiddlewareProbe {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            assert_eq!(context.api.as_str(), OPENROUTER_IMAGES_API_ID);
            headers.insert(
                "x-image-transform",
                http::HeaderValue::from_static("applied"),
            );
            self.header_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl LocalHeaderTransform for ImageMiddlewareProbe {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            assert_eq!(context.api.as_str(), OPENROUTER_IMAGES_API_ID);
            headers.insert(
                "x-image-transform",
                http::HeaderValue::from_static("applied"),
            );
            self.header_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl ErasedPayloadTransform for ImageMiddlewareProbe {
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> SendBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            assert_eq!(context.api.as_str(), OPENROUTER_IMAGES_API_ID);
            self.payload_calls.fetch_add(1, Ordering::SeqCst);
            Ok(PayloadTransformDisposition::Replace(ProviderPayload::json(
                br#"{"middleware":"send-replacement"}"#.to_vec(),
            )))
        })
    }
}

impl LocalErasedPayloadTransform for ImageMiddlewareProbe {
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            assert_eq!(context.api.as_str(), OPENROUTER_IMAGES_API_ID);
            self.payload_calls.fetch_add(1, Ordering::SeqCst);
            Ok(PayloadTransformDisposition::Replace(ProviderPayload::json(
                br#"{"middleware":"local-replacement"}"#.to_vec(),
            )))
        })
    }
}

#[test]
fn openrouter_images_header_and_payload_middleware_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/images-models.ts header merging and
    // packages/ai/src/api/openrouter-images.ts onPayload replacement.
    let header_calls = Arc::new(AtomicUsize::new(0));
    let payload_calls = Arc::new(AtomicUsize::new(0));
    let send_probe = Arc::new(ImageMiddlewareProbe {
        header_calls: Arc::clone(&header_calls),
        payload_calls: Arc::clone(&payload_calls),
    });
    let send_transport = Arc::new(FixtureTransport::with_response(
        200,
        fixture_file("text-only", "response.body.json"),
    ));
    let send_provider = agentprism_openrouter::openrouter_provider(send_transport.clone())
        .expect("send OpenRouter provider");
    let send = Models::builder()
        .provider(send_provider)
        .header_transform(Arc::clone(&send_probe) as Arc<dyn HeaderTransform>)
        .erased_payload_transform(send_probe as Arc<dyn ErasedPayloadTransform>)
        .build()
        .expect("Models");
    let send_model = configured_image_model(&send);
    let send_result = futures_executor::block_on(send.generate_images(
        send_model,
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);
    let send_requests = lock(&send_transport.requests);
    assert_eq!(
        send_requests[0].body,
        br#"{"middleware":"send-replacement"}"#
    );
    assert_eq!(send_requests[0].headers["x-image-transform"], "applied");
    drop(send_requests);

    let local_probe = Rc::new(ImageMiddlewareProbe {
        header_calls: Arc::clone(&header_calls),
        payload_calls: Arc::clone(&payload_calls),
    });
    let local_transport = Rc::new(FixtureTransport::with_response(
        200,
        fixture_file("text-only", "response.body.json"),
    ));
    let local_provider = agentprism_openrouter::local_openrouter_provider(local_transport.clone())
        .expect("local OpenRouter provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .header_transform(Rc::clone(&local_probe) as Rc<dyn LocalHeaderTransform>)
        .erased_payload_transform(local_probe as Rc<dyn LocalErasedPayloadTransform>)
        .build()
        .expect("LocalModels");
    let local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local published OpenRouter image model");
    let local_result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    let local_requests = lock(&local_transport.requests);
    assert_eq!(
        local_requests[0].body,
        br#"{"middleware":"local-replacement"}"#
    );
    assert_eq!(local_requests[0].headers["x-image-transform"], "applied");
    assert_eq!(header_calls.load(Ordering::SeqCst), 2);
    assert_eq!(payload_calls.load(Ordering::SeqCst), 2);
}

struct CancelThenErrorPayload {
    cancellation: CancellationToken,
}

impl ErasedPayloadTransform for CancelThenErrorPayload {
    fn transform<'a>(
        &'a self,
        _context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> SendBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            Err(MiddlewareError::new(
                "image_payload_callback",
                "payload callback failed after cancellation",
            ))
        })
    }
}

impl LocalErasedPayloadTransform for CancelThenErrorPayload {
    fn transform<'a>(
        &'a self,
        _context: ErasedPayloadContext<'a>,
        _payload: &'a mut ProviderPayload,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            Err(MiddlewareError::new(
                "image_payload_callback",
                "local payload callback failed after cancellation",
            ))
        })
    }
}

#[test]
fn openrouter_images_payload_callback_error_after_cancellation_is_aborted_send_and_local_pi_exact()
{
    // Pi basis: packages/ai/src/api/openrouter-images.ts wraps onPayload in
    // the same catch that classifies the terminal reason from signal.aborted.
    let send_cancellation = CancellationToken::new();
    let send_transport = Arc::new(FixtureTransport::default());
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(send_transport.clone())
                .expect("send callback provider"),
        )
        .erased_payload_transform(Arc::new(CancelThenErrorPayload {
            cancellation: send_cancellation.clone(),
        }))
        .build()
        .expect("send callback Models");
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        send_cancellation,
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        send_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("payload callback failed"))
    );
    assert!(lock(&send_transport.requests).is_empty());

    let local_cancellation = CancellationToken::new();
    let local_transport = Rc::new(FixtureTransport::default());
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(local_transport.clone())
                .expect("local callback provider"),
        )
        .erased_payload_transform(Rc::new(CancelThenErrorPayload {
            cancellation: local_cancellation.clone(),
        }))
        .build()
        .expect("local callback Models");
    let local_result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            explicit_options(),
            local_cancellation,
        ),
    );
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        local_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("local payload callback failed"))
    );
    assert!(lock(&local_transport.requests).is_empty());
}

struct AuthorizationOnlyTransform;

impl HeaderTransform for AuthorizationOnlyTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        _context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer transformed-without-key"),
        );
        Box::pin(async { Ok(()) })
    }
}

struct DeleteImageAuthorizationTransform;

impl HeaderTransform for DeleteImageAuthorizationTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        _context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.remove(http::header::AUTHORIZATION);
        Box::pin(async { Ok(()) })
    }
}

impl LocalHeaderTransform for DeleteImageAuthorizationTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        _context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.remove(http::header::AUTHORIZATION);
        Box::pin(async { Ok(()) })
    }
}

impl LocalHeaderTransform for AuthorizationOnlyTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        _headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async { Ok(()) })
    }

    fn transform_image<'a>(
        &'a self,
        _context: ImageHeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer local-transformed-without-key"),
        );
        Box::pin(async { Ok(()) })
    }
}

fn authorization_header_spec(value: &str) -> HeaderMapSpec {
    BTreeMap::from([("Authorization".to_owned(), Some(value.to_owned()))])
}

#[test]
fn openrouter_images_auth_eligibility_ignores_final_authorization_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts requires the
    // separately supplied options.apiKey before constructing the client.
    // Model, explicit, and later transformed Authorization defaults do not
    // substitute for that key.
    let send_transport = Arc::new(FixtureTransport::default());
    let send_provider =
        agentprism_openrouter::openrouter_provider(send_transport.clone()).expect("provider");
    let send = Models::builder()
        .provider(send_provider)
        .header_transform(Arc::new(AuthorizationOnlyTransform))
        .build()
        .expect("Models");
    let mut send_model = configured_image_model(&send);
    send_model.headers = authorization_header_spec("Bearer model-without-key");
    let mut send_options = ImageGenerationOptions::default();
    send_options.request.headers = authorization_header_spec("Bearer request-without-key");
    let send_result = futures_executor::block_on(send.generate_images(
        send_model,
        ImageGenerationContext::default(),
        send_options,
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Error);
    assert!(
        send_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("No API key"))
    );
    assert!(lock(&send_transport.requests).is_empty());

    let local_transport = Rc::new(FixtureTransport::default());
    let local_provider = agentprism_openrouter::local_openrouter_provider(local_transport.clone())
        .expect("local provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .header_transform(Rc::new(AuthorizationOnlyTransform))
        .build()
        .expect("LocalModels");
    let mut local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local image model");
    local_model.headers = authorization_header_spec("Bearer local-model-without-key");
    let mut local_options = ImageGenerationOptions::default();
    local_options.request.headers = authorization_header_spec("Bearer local-request-without-key");
    let local_result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        local_options,
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Error);
    assert!(
        local_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("No API key"))
    );
    assert!(lock(&local_transport.requests).is_empty());
}

#[test]
fn openrouter_images_header_precedence_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/images-models.ts merges resolved auth before
    // explicit options, while packages/ai/src/api/openrouter-images.ts places
    // that merged map after model headers in OpenAI defaultHeaders.
    let response = fixture_file("text-only", "response.body.json");
    let send_transport = Arc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([
            (200, response.clone()),
            (200, response.clone()),
        ])),
    });
    let send = configured_models(send_transport.clone());
    let mut send_model = configured_image_model(&send);
    send_model.headers = authorization_header_spec("Bearer model-default");
    let first = futures_executor::block_on(send.generate_images(
        send_model.clone(),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(first.stop_reason, ImageGenerationStopReason::Stop);
    let mut explicit = explicit_options();
    explicit.request.headers = authorization_header_spec("Bearer explicit-header");
    let second = futures_executor::block_on(send.generate_images(
        send_model,
        ImageGenerationContext::default(),
        explicit,
        CancellationToken::new(),
    ));
    assert_eq!(second.stop_reason, ImageGenerationStopReason::Stop);
    let requests = lock(&send_transport.requests);
    assert_eq!(
        requests[0].headers[http::header::AUTHORIZATION],
        "Bearer explicit-fixture-key"
    );
    assert_eq!(
        requests[1].headers[http::header::AUTHORIZATION],
        "Bearer explicit-header"
    );
    drop(requests);

    let local_transport = Rc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([(200, response.clone()), (200, response)])),
    });
    let local_provider = agentprism_openrouter::local_openrouter_provider(local_transport.clone())
        .expect("local provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("LocalModels");
    let mut local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local image model");
    local_model.headers = authorization_header_spec("Bearer local-model-default");
    let first = futures_executor::block_on(local.generate_images(
        local_model.clone(),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(first.stop_reason, ImageGenerationStopReason::Stop);
    let mut explicit = explicit_options();
    explicit.request.headers = authorization_header_spec("Bearer local-explicit-header");
    let second = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        explicit,
        CancellationToken::new(),
    ));
    assert_eq!(second.stop_reason, ImageGenerationStopReason::Stop);
    let requests = lock(&local_transport.requests);
    assert_eq!(
        requests[0].headers[http::header::AUTHORIZATION],
        "Bearer explicit-fixture-key"
    );
    assert_eq!(
        requests[1].headers[http::header::AUTHORIZATION],
        "Bearer local-explicit-header"
    );
}

#[test]
fn openrouter_images_deleted_logical_authorization_reveals_sdk_default_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts passes apiKey as an
    // OpenAI client option separately from defaultHeaders. Removing a logical
    // Authorization override therefore reveals the SDK bearer default.
    let response = fixture_file("text-only", "response.body.json");
    let send_transport = Arc::new(FixtureTransport::with_response(200, response.clone()));
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(send_transport.clone())
                .expect("send provider"),
        )
        .header_transform(Arc::new(DeleteImageAuthorizationTransform))
        .build()
        .expect("send Models");
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(
        lock(&send_transport.requests)[0].headers[http::header::AUTHORIZATION],
        "Bearer explicit-fixture-key"
    );

    let local_transport = Rc::new(FixtureTransport::with_response(200, response));
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(local_transport.clone())
                .expect("local provider"),
        )
        .header_transform(Rc::new(DeleteImageAuthorizationTransform))
        .build()
        .expect("local Models");
    let local_result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            explicit_options(),
            CancellationToken::new(),
        ),
    );
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(
        lock(&local_transport.requests)[0].headers[http::header::AUTHORIZATION],
        "Bearer explicit-fixture-key"
    );
}

#[test]
fn openrouter_images_data_url_extraction_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts applies
    // /^data:([^;]+);base64,(.+)$/ to both string and object image_url forms.
    let response = br#"{"id":"data-url-shapes","choices":[{"message":{"content":null,"images":[{"image_url":"data:image/png;base64,VALID"},{"image_url":{"url":"data:image/webp;base64,OBJECT"}},{"image_url":"data:image/png;charset=utf-8;base64,METADATA"},{"image_url":"data:image/png;base64,"},{"image_url":"DATA:image/png;base64,UPPER"},{"image_url":"data:image/png;base64,LINE\nBREAK"},{"image_url":"data:image/png;base64,TRAILING\n"},{"image_url":"data:image/png;base64,%%%"}]}}]}"#.to_vec();
    let expected = vec![
        ImageGenerationContent::image("VALID", "image/png"),
        ImageGenerationContent::image("OBJECT", "image/webp"),
        ImageGenerationContent::image("%%%", "image/png"),
    ];

    let send_transport = Arc::new(FixtureTransport::with_response(200, response.clone()));
    let send = configured_models(send_transport);
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(send_result.output, expected);

    let local_transport = Rc::new(FixtureTransport::with_response(200, response));
    let local_provider =
        agentprism_openrouter::local_openrouter_provider(local_transport).expect("local provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("LocalModels");
    let local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local image model");
    let local_result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(local_result.output, expected);
}

#[test]
fn openrouter_images_hermetic_generation_scenarios() {
    // Pi basis: packages/ai/test/images.test.ts basic generation,
    // text-and-image output, and image-input scenarios.
    for case in CASES {
        let canonical: Value =
            serde_json::from_slice(&fixture_file(case, "canonical.json")).expect("canonical JSON");
        let result = agentprism_ai::decode_openrouter_images_response(
            &fixture_model(&canonical),
            &fixture_file(case, "response.body.json"),
        )
        .expect("captured response decodes");
        assert_eq!(
            result.stop_reason,
            ImageGenerationStopReason::Stop,
            "case {case}"
        );
        assert!(
            result
                .output
                .iter()
                .any(|item| matches!(item, ImageGenerationContent::Image { .. })),
            "case {case}"
        );
        if *case == "text-and-image-output" {
            assert!(
                result
                    .output
                    .iter()
                    .any(|item| matches!(item, ImageGenerationContent::Text { .. }))
            );
        }
    }
}

#[test]
fn openrouter_images_provider_error_body_passthrough() {
    // Pi basis: packages/ai/test/provider-error-body-passthrough.test.ts.
    let transport = Arc::new(FixtureTransport::with_response(
        403,
        br#"{"error":{"message":"blocked by gateway WAF"}}"#.to_vec(),
    ));
    let models = configured_models(transport);
    let model = configured_image_model(&models);
    let result = futures_executor::block_on(models.generate_images(
        model,
        ImageGenerationContext {
            input: vec![ImageGenerationContent::text("draw it")],
        },
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    let message = result.error_message.expect("error message");
    assert!(message.contains("403"));
    assert!(message.contains("blocked by gateway WAF"));
}

#[derive(Default)]
struct AlreadyAbortedTransport {
    calls: AtomicUsize,
    received_cancelled_token: AtomicBool,
}

impl HttpTransport for AlreadyAbortedTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received_cancelled_token
            .store(cancellation.is_cancelled(), Ordering::SeqCst);
        Box::pin(async { Err(TransportError::new("aborted", "Request aborted")) })
    }
}

impl LocalHttpTransport for AlreadyAbortedTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received_cancelled_token
            .store(cancellation.is_cancelled(), Ordering::SeqCst);
        Box::pin(async { Err(TransportError::new("aborted", "Request aborted")) })
    }
}

#[test]
fn openrouter_images_already_aborted_direct_adapter_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/openrouter-images.test.ts "passes through
    // abort signal and returns aborted result", plus OpenAI SDK 6.40.0 request
    // admission: the SDK closure is invoked but an already-aborted signal
    // rejects before the injected fetch. This is direct API-family dispatch,
    // before ImagesModels auth orchestration.
    let send_transport = Arc::new(AlreadyAbortedTransport::default());
    let send_api = agentprism_ai::OpenRouterImagesApi::new(send_transport.clone());
    let send_cancellation = CancellationToken::new();
    send_cancellation.cancel();
    let send_result = futures_executor::block_on(ImagesApi::generate(
        &send_api,
        direct_send_image_request(synthetic_model("openrouter", "send-aborted"), None),
        send_cancellation,
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert_eq!(
        send_result.error_message.as_deref(),
        Some("Request aborted")
    );
    assert_eq!(send_transport.calls.load(Ordering::SeqCst), 0);
    assert!(
        !send_transport
            .received_cancelled_token
            .load(Ordering::SeqCst)
    );

    let local_transport = Rc::new(AlreadyAbortedTransport::default());
    let local_api = agentprism_ai::LocalOpenRouterImagesApi::new(local_transport.clone());
    let local_cancellation = CancellationToken::new();
    local_cancellation.cancel();
    let local_result = futures_executor::block_on(LocalImagesApi::generate(
        &local_api,
        direct_local_image_request(synthetic_model("openrouter", "local-aborted"), None),
        local_cancellation,
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert_eq!(
        local_result.error_message.as_deref(),
        Some("Request aborted")
    );
    assert_eq!(local_transport.calls.load(Ordering::SeqCst), 0);
    assert!(
        !local_transport
            .received_cancelled_token
            .load(Ordering::SeqCst)
    );
}

#[test]
fn openrouter_images_abort_is_in_band() {
    // Pi basis: packages/ai/src/images-models.ts catches cancellation during
    // auth as an error; packages/ai/test/openrouter-images.test.ts covers the
    // adapter's aborted result after provider dispatch.
    let transport = Arc::new(FixtureTransport::default());
    let models = configured_models(Arc::clone(&transport));
    let model = configured_image_model(&models);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = futures_executor::block_on(models.generate_images(
        model,
        ImageGenerationContext::default(),
        explicit_options(),
        cancellation,
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));
    assert!(lock(&transport.requests).is_empty());

    let pending = Arc::new(PendingBodyTransport);
    let models = configured_models_with_transport(Arc::clone(&pending));
    let model = configured_image_model(&models);
    let cancellation = CancellationToken::new();
    let pending_result = futures_executor::block_on(async {
        let generation = models.generate_images(
            model,
            ImageGenerationContext::default(),
            explicit_options(),
            cancellation.clone(),
        );
        let abort = async {
            let mut yielded = false;
            futures_util::future::poll_fn(move |context| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            cancellation.cancel();
        };
        futures_util::future::join(generation, abort).await.0
    });
    assert_eq!(
        pending_result.stop_reason,
        ImageGenerationStopReason::Aborted
    );
    assert_eq!(
        pending_result.error_message.as_deref(),
        Some("Request aborted")
    );
}

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
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

struct SuccessfulBodyFailureTransport {
    attempts: Arc<AtomicUsize>,
}

impl HttpTransport for SuccessfulBodyFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new(
                        "body",
                        "successful-response body failed",
                    ))
                })),
            })
        })
    }
}

impl LocalHttpTransport for SuccessfulBodyFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Ok(LocalHttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async {
                    Err(TransportError::new(
                        "body",
                        "local successful-response body failed",
                    ))
                })),
            })
        })
    }
}

struct NonSuccessBodyFailureThenSuccessTransport {
    attempts: Arc<AtomicUsize>,
    success_body: Vec<u8>,
}

impl HttpTransport for NonSuccessBodyFailureThenSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Ok(HttpResponse {
                    status: 500,
                    headers: json_response_headers(),
                    diagnostics: Vec::new(),
                    notify_observers: true,
                    decode_non_success: false,
                    body: Box::pin(futures_util::stream::once(async {
                        Err(TransportError::new(
                            "body",
                            "non-success response body failed",
                        ))
                    })),
                });
            }
            Ok(HttpResponse::from_bytes(
                200,
                json_response_headers(),
                self.success_body.clone(),
            ))
        })
    }
}

impl LocalHttpTransport for NonSuccessBodyFailureThenSuccessTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Ok(LocalHttpResponse {
                    status: 500,
                    headers: json_response_headers(),
                    diagnostics: Vec::new(),
                    notify_observers: true,
                    decode_non_success: false,
                    body: Box::pin(futures_util::stream::once(async {
                        Err(TransportError::new(
                            "body",
                            "local non-success response body failed",
                        ))
                    })),
                });
            }
            Ok(LocalHttpResponse::from_bytes(
                200,
                json_response_headers(),
                self.success_body.clone(),
            ))
        })
    }
}

struct SdkResponseShapeTransport {
    status: u16,
    content_type: &'static str,
    content_length: Option<&'static str>,
    body: Vec<u8>,
    body_polled: Arc<AtomicBool>,
}

fn sdk_response_body(
    body: Vec<u8>,
    body_polled: Arc<AtomicBool>,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, TransportError>> + 'static {
    let mut body = Some(body);
    futures_util::stream::poll_fn(move |_| {
        let Some(body) = body.take() else {
            return Poll::Ready(None);
        };
        body_polled.store(true, Ordering::SeqCst);
        Poll::Ready(Some(Ok(body)))
    })
}

impl HttpTransport for SdkResponseShapeTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static(self.content_type),
            );
            if let Some(content_length) = self.content_length {
                headers.insert(
                    http::header::CONTENT_LENGTH,
                    http::HeaderValue::from_static(content_length),
                );
            }
            Ok(HttpResponse {
                status: self.status,
                headers,
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(sdk_response_body(
                    self.body.clone(),
                    Arc::clone(&self.body_polled),
                )),
            })
        })
    }
}

impl LocalHttpTransport for SdkResponseShapeTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static(self.content_type),
            );
            if let Some(content_length) = self.content_length {
                headers.insert(
                    http::header::CONTENT_LENGTH,
                    http::HeaderValue::from_static(content_length),
                );
            }
            Ok(LocalHttpResponse {
                status: self.status,
                headers,
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(sdk_response_body(
                    self.body.clone(),
                    Arc::clone(&self.body_polled),
                )),
            })
        })
    }
}

struct ProjectionObserver {
    calls: Arc<AtomicUsize>,
}

impl ResponseObserver for ProjectionObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        _response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl LocalResponseObserver for ProjectionObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        _response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct CancellingImageObserver {
    cancellation: CancellationToken,
    fail: bool,
}

impl ResponseObserver for CancellingImageObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        _response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            if self.fail {
                Err(MiddlewareError::new(
                    "image_response_callback",
                    "response callback failed after cancellation",
                ))
            } else {
                Ok(())
            }
        })
    }
}

impl LocalResponseObserver for CancellingImageObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        _response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            if self.fail {
                Err(MiddlewareError::new(
                    "image_response_callback",
                    "local response callback failed after cancellation",
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[test]
fn openrouter_images_response_callback_error_after_cancellation_is_aborted_send_and_local_pi_exact()
{
    // Pi basis: packages/ai/src/api/openrouter-images.ts invokes onResponse
    // inside the catch whose terminal reason reads signal.aborted.
    let response = fixture_file("text-only", "response.body.json");
    let send_cancellation = CancellationToken::new();
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(Arc::new(FixtureTransport::with_response(
                200,
                response.clone(),
            )))
            .expect("send observer provider"),
        )
        .response_observer(Arc::new(CancellingImageObserver {
            cancellation: send_cancellation.clone(),
            fail: true,
        }))
        .build()
        .expect("send observer Models");
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        send_cancellation,
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        send_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("response callback failed"))
    );

    let local_cancellation = CancellationToken::new();
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(Rc::new(
                FixtureTransport::with_response(200, response),
            ))
            .expect("local observer provider"),
        )
        .response_observer(Rc::new(CancellingImageObserver {
            cancellation: local_cancellation.clone(),
            fail: true,
        }))
        .build()
        .expect("local observer Models");
    let local_result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            explicit_options(),
            local_cancellation,
        ),
    );
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        local_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("local response callback failed"))
    );
}

#[test]
fn openrouter_images_projection_error_after_observer_cancellation_is_aborted_send_and_local_pi_exact()
 {
    // Pi basis: packages/ai/src/api/openrouter-images.ts projects the parsed
    // response after onResponse but classifies every later throw from the same
    // catch using signal.aborted.
    let send_cancellation = CancellationToken::new();
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(Arc::new(FixtureTransport::with_response(
                200,
                b"{}".to_vec(),
            )))
            .expect("send projection provider"),
        )
        .response_observer(Arc::new(CancellingImageObserver {
            cancellation: send_cancellation.clone(),
            fail: false,
        }))
        .build()
        .expect("send projection Models");
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        send_cancellation,
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        send_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("choices"))
    );

    let local_cancellation = CancellationToken::new();
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(Rc::new(
                FixtureTransport::with_response(200, b"{}".to_vec()),
            ))
            .expect("local projection provider"),
        )
        .response_observer(Rc::new(CancellingImageObserver {
            cancellation: local_cancellation.clone(),
            fail: false,
        }))
        .build()
        .expect("local projection Models");
    let local_result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            explicit_options(),
            local_cancellation,
        ),
    );
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Aborted);
    assert!(
        local_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("choices"))
    );
}

#[test]
fn openrouter_images_projection_failure_retains_assigned_fields_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts mutates the output's
    // responseId, usage, and text before iterating images. A null image throws
    // on `image.image_url`, and the surrounding catch terminalizes that same
    // partially populated output object.
    let response = br#"{"id":"retained-id","usage":{"prompt_tokens":7,"completion_tokens":3},"choices":[{"message":{"content":"retained text","images":[null]}}]}"#.to_vec();

    let send_transport = Arc::new(FixtureTransport::with_response(200, response.clone()));
    let send = configured_models(send_transport);
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(send_result.response_id.as_deref(), Some("retained-id"));
    assert_eq!(
        send_result
            .usage
            .as_ref()
            .expect("send usage")
            .total_tokens(),
        10
    );
    assert_eq!(
        send_result.output,
        [ImageGenerationContent::text("retained text")]
    );
    assert!(
        send_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("image_url"))
    );

    let local_transport = Rc::new(FixtureTransport::with_response(200, response));
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(local_transport)
                .expect("local partial projection provider"),
        )
        .build()
        .expect("local partial projection Models");
    let local_result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            explicit_options(),
            CancellationToken::new(),
        ),
    );
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(local_result.response_id.as_deref(), Some("retained-id"));
    assert_eq!(
        local_result
            .usage
            .as_ref()
            .expect("local usage")
            .total_tokens(),
        10
    );
    assert_eq!(
        local_result.output,
        [ImageGenerationContent::text("retained text")]
    );
    assert!(
        local_result
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("image_url"))
    );
}

#[test]
fn openrouter_images_provider_errors_retry_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.3 `retry_http_500_through_599` specialized
    // to OpenRouter Images. Pi basis: packages/ai/src/api/openrouter-images.ts
    // and packages/ai/src/utils/provider-retry.ts.
    let response = fixture_file("text-only", "response.body.json");
    let send_transport = Arc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([
            (
                503,
                br#"{"error":{"message":"temporarily unavailable"}}"#.to_vec(),
            ),
            (200, response.clone()),
        ])),
    });
    let mut send_provider = agentprism_openrouter::openrouter_provider(send_transport.clone())
        .expect("send OpenRouter provider");
    send_provider.retry_policy.exponential_base = Duration::ZERO;
    send_provider.retry_policy.exponential_cap = Duration::ZERO;
    let send = Models::builder()
        .provider(send_provider)
        .build()
        .expect("Models");
    let mut send_options = explicit_options();
    send_options.request.max_retries = Some(1);
    let send_result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        send_options,
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(lock(&send_transport.requests).len(), 2);

    let local_transport = Rc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([
            (500, br#"{"error":{"message":"retry me"}}"#.to_vec()),
            (200, response),
        ])),
    });
    let mut local_provider =
        agentprism_openrouter::local_openrouter_provider(local_transport.clone())
            .expect("local OpenRouter provider");
    local_provider.retry_policy.exponential_base = Duration::ZERO;
    local_provider.retry_policy.exponential_cap = Duration::ZERO;
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("LocalModels");
    let local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local image model");
    let mut local_options = explicit_options();
    local_options.request.max_retries = Some(1);
    let local_result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        local_options,
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(lock(&local_transport.requests).len(), 2);
}

#[test]
fn openrouter_images_non_success_body_failure_retries_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.3 `retry_http_500_through_599`. Pi basis:
    // OpenAI SDK 6.40.0 client.ts turns a non-2xx response-body read failure
    // into an APIError retaining status/headers, and openrouter-images.ts puts
    // `.withResponse()` inside retryProviderRequest.
    let response = fixture_file("text-only", "response.body.json");
    let send_attempts = Arc::new(AtomicUsize::new(0));
    let send_transport = Arc::new(NonSuccessBodyFailureThenSuccessTransport {
        attempts: Arc::clone(&send_attempts),
        success_body: response.clone(),
    });
    let mut send_provider = agentprism_openrouter::openrouter_provider(send_transport)
        .expect("send non-success body-failure provider");
    send_provider.retry_policy.exponential_base = Duration::ZERO;
    send_provider.retry_policy.exponential_cap = Duration::ZERO;
    let send = Models::builder()
        .provider(send_provider)
        .build()
        .expect("send non-success body-failure Models");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        options,
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(send_attempts.load(Ordering::SeqCst), 2);

    let local_attempts = Arc::new(AtomicUsize::new(0));
    let local_transport = Rc::new(NonSuccessBodyFailureThenSuccessTransport {
        attempts: Arc::clone(&local_attempts),
        success_body: response,
    });
    let mut local_provider = agentprism_openrouter::local_openrouter_provider(local_transport)
        .expect("local non-success body-failure provider");
    local_provider.retry_policy.exponential_base = Duration::ZERO;
    local_provider.retry_policy.exponential_cap = Duration::ZERO;
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("local non-success body-failure Models");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(
        local.generate_images(
            local
                .image_model(&ModelRef::new(
                    "openrouter",
                    "google/gemini-2.5-flash-image",
                ))
                .expect("local image model"),
            ImageGenerationContext::default(),
            options,
            CancellationToken::new(),
        ),
    );
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(local_attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn openrouter_images_sdk_content_type_and_204_parsing_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.4 response-observer ordering. Pi basis:
    // openrouter-images.ts `.withResponse()` plus OpenAI SDK 6.40.0
    // internal/parse.ts: non-JSON media types return text and 204 maps to null
    // without consuming the body; both are parsed before onResponse and fail
    // only during Pi's AssistantImages projection.
    let response = fixture_file("text-only", "response.body.json");
    let send_calls = Arc::new(AtomicUsize::new(0));
    for (status, content_type, content_length, body, expected_body_poll) in [
        (200, "text/plain", None, response.clone(), true),
        (
            204,
            "application/json",
            None,
            b"must not be read".to_vec(),
            false,
        ),
        (
            200,
            "application/json",
            Some("0"),
            b"must not be read".to_vec(),
            false,
        ),
    ] {
        let body_polled = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(SdkResponseShapeTransport {
            status,
            content_type,
            content_length,
            body,
            body_polled: Arc::clone(&body_polled),
        });
        let models = Models::builder()
            .provider(
                agentprism_openrouter::openrouter_provider(transport)
                    .expect("send SDK-shape provider"),
            )
            .response_observer(Arc::new(ProjectionObserver {
                calls: Arc::clone(&send_calls),
            }))
            .build()
            .expect("send SDK-shape Models");
        let result = futures_executor::block_on(models.generate_images(
            configured_image_model(&models),
            ImageGenerationContext::default(),
            explicit_options(),
            CancellationToken::new(),
        ));
        assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
        assert_eq!(body_polled.load(Ordering::SeqCst), expected_body_poll);
    }
    assert_eq!(send_calls.load(Ordering::SeqCst), 3);

    let local_calls = Arc::new(AtomicUsize::new(0));
    for (status, content_type, content_length, body, expected_body_poll) in [
        (200, "text/plain", None, response.clone(), true),
        (
            204,
            "application/json",
            None,
            b"must not be read".to_vec(),
            false,
        ),
        (
            200,
            "application/json",
            Some("0"),
            b"must not be read".to_vec(),
            false,
        ),
    ] {
        let body_polled = Arc::new(AtomicBool::new(false));
        let transport = Rc::new(SdkResponseShapeTransport {
            status,
            content_type,
            content_length,
            body,
            body_polled: Arc::clone(&body_polled),
        });
        let models = LocalModels::builder()
            .provider(
                agentprism_openrouter::local_openrouter_provider(transport)
                    .expect("local SDK-shape provider"),
            )
            .response_observer(Rc::new(ProjectionObserver {
                calls: Arc::clone(&local_calls),
            }))
            .build()
            .expect("local SDK-shape Models");
        let result = futures_executor::block_on(
            models.generate_images(
                models
                    .image_model(&ModelRef::new(
                        "openrouter",
                        "google/gemini-2.5-flash-image",
                    ))
                    .expect("local SDK-shape image model"),
                ImageGenerationContext::default(),
                explicit_options(),
                CancellationToken::new(),
            ),
        );
        assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
        assert_eq!(body_polled.load(Ordering::SeqCst), expected_body_poll);
    }
    assert_eq!(local_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn openrouter_images_success_body_and_json_fail_once_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.3 `retry_transport_failure_without_status`,
    // with the pinned SDK-shape exception: successful-response body and JSON
    // failures lack `status`/`headers` and are not ProviderError values. Pi
    // basis: openrouter-images.ts `.withResponse()` and provider-retry.ts
    // `isProviderError`.
    let send_body_attempts = Arc::new(AtomicUsize::new(0));
    let send_body_transport = Arc::new(SuccessfulBodyFailureTransport {
        attempts: Arc::clone(&send_body_attempts),
    });
    let mut send_body_provider = agentprism_openrouter::openrouter_provider(send_body_transport)
        .expect("send body-failure provider");
    send_body_provider.retry_policy.exponential_base = Duration::ZERO;
    send_body_provider.retry_policy.exponential_cap = Duration::ZERO;
    let send_body = Models::builder()
        .provider(send_body_provider)
        .build()
        .expect("send body-failure Models");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(send_body.generate_images(
        configured_image_model(&send_body),
        ImageGenerationContext::default(),
        options,
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(send_body_attempts.load(Ordering::SeqCst), 1);

    let send_json_transport = Arc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([
            (200, br#"{"#.to_vec()),
            (200, fixture_file("text-only", "response.body.json")),
        ])),
    });
    let mut send_json_provider =
        agentprism_openrouter::openrouter_provider(send_json_transport.clone())
            .expect("send JSON-failure provider");
    send_json_provider.retry_policy.exponential_base = Duration::ZERO;
    send_json_provider.retry_policy.exponential_cap = Duration::ZERO;
    let send_json = Models::builder()
        .provider(send_json_provider)
        .build()
        .expect("send JSON-failure Models");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(send_json.generate_images(
        configured_image_model(&send_json),
        ImageGenerationContext::default(),
        options,
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(lock(&send_json_transport.requests).len(), 1);

    let local_body_attempts = Arc::new(AtomicUsize::new(0));
    let local_body_transport = Rc::new(SuccessfulBodyFailureTransport {
        attempts: Arc::clone(&local_body_attempts),
    });
    let mut local_body_provider =
        agentprism_openrouter::local_openrouter_provider(local_body_transport)
            .expect("local body-failure provider");
    local_body_provider.retry_policy.exponential_base = Duration::ZERO;
    local_body_provider.retry_policy.exponential_cap = Duration::ZERO;
    let local_body = LocalModels::builder()
        .provider(local_body_provider)
        .build()
        .expect("local body-failure Models");
    let local_model = local_body
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local body-failure image model");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(local_body.generate_images(
        local_model,
        ImageGenerationContext::default(),
        options,
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(local_body_attempts.load(Ordering::SeqCst), 1);

    let local_json_transport = Rc::new(FixtureTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::from([
            (200, br#"{"#.to_vec()),
            (200, fixture_file("text-only", "response.body.json")),
        ])),
    });
    let mut local_json_provider =
        agentprism_openrouter::local_openrouter_provider(local_json_transport.clone())
            .expect("local JSON-failure provider");
    local_json_provider.retry_policy.exponential_base = Duration::ZERO;
    local_json_provider.retry_policy.exponential_cap = Duration::ZERO;
    let local_json = LocalModels::builder()
        .provider(local_json_provider)
        .build()
        .expect("local JSON-failure Models");
    let local_model = local_json
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local JSON-failure image model");
    let mut options = explicit_options();
    options.request.max_retries = Some(1);
    let result = futures_executor::block_on(local_json.generate_images(
        local_model,
        ImageGenerationContext::default(),
        options,
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(lock(&local_json_transport.requests).len(), 1);
}

#[test]
fn openrouter_images_observer_precedes_semantic_projection_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.4 response-observer ordering, specialized to
    // the non-streaming SDK boundary. Pi basis: openrouter-images.ts invokes
    // onResponse after `.withResponse()` parsing and before choices projection.
    let send_calls = Arc::new(AtomicUsize::new(0));
    let send_transport = Arc::new(FixtureTransport::with_response(200, b"{}".to_vec()));
    let send = Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(send_transport.clone())
                .expect("send projection provider"),
        )
        .response_observer(Arc::new(ProjectionObserver {
            calls: Arc::clone(&send_calls),
        }))
        .build()
        .expect("send projection Models");
    let result = futures_executor::block_on(send.generate_images(
        configured_image_model(&send),
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(lock(&send_transport.requests).len(), 1);
    assert_eq!(send_calls.load(Ordering::SeqCst), 1);

    let local_calls = Arc::new(AtomicUsize::new(0));
    let local_transport = Rc::new(FixtureTransport::with_response(200, b"{}".to_vec()));
    let local = LocalModels::builder()
        .provider(
            agentprism_openrouter::local_openrouter_provider(local_transport.clone())
                .expect("local projection provider"),
        )
        .response_observer(Rc::new(ProjectionObserver {
            calls: Arc::clone(&local_calls),
        }))
        .build()
        .expect("local projection Models");
    let local_model = local
        .image_model(&ModelRef::new(
            "openrouter",
            "google/gemini-2.5-flash-image",
        ))
        .expect("local projection image model");
    let result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        explicit_options(),
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(lock(&local_transport.requests).len(), 1);
    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
}

struct SecretFailureTransport {
    secret: &'static str,
    body_failure: bool,
}

impl HttpTransport for SecretFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if !self.body_failure {
                return Err(TransportError::new(
                    "establishment",
                    format!("transport exposed {}", self.secret),
                ));
            }
            let secret = self.secret;
            Ok(HttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async move {
                    Err(TransportError::new(
                        "body",
                        format!("body exposed {secret}"),
                    ))
                })),
            })
        })
    }
}

impl LocalHttpTransport for SecretFailureTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            if !self.body_failure {
                return Err(TransportError::new(
                    "establishment",
                    format!("local transport exposed {}", self.secret),
                ));
            }
            let secret = self.secret;
            Ok(LocalHttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(futures_util::stream::once(async move {
                    Err(TransportError::new(
                        "body",
                        format!("local body exposed {secret}"),
                    ))
                })),
            })
        })
    }
}

#[test]
fn openrouter_images_transport_and_body_errors_are_redacted_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/openrouter-images.ts catch path and
    // packages/ai/test/provider-error-body-passthrough.test.ts secret-safe
    // normalized provider error boundary.
    const SECRET: &str = "openrouter-image-secret-sentinel";
    for body_failure in [false, true] {
        let send_transport = Arc::new(SecretFailureTransport {
            secret: SECRET,
            body_failure,
        });
        let send = configured_models_with_transport(send_transport);
        let mut options = ImageGenerationOptions::default();
        options.auth.api_key = Some(SecretString::new(SECRET));
        let result = futures_executor::block_on(send.generate_images(
            configured_image_model(&send),
            ImageGenerationContext::default(),
            options,
            CancellationToken::new(),
        ));
        let message = result.error_message.expect("send terminal error");
        assert!(!message.contains(SECRET), "send leaked secret: {message}");
        assert!(message.contains("[REDACTED]"));

        let local_transport = Rc::new(SecretFailureTransport {
            secret: SECRET,
            body_failure,
        });
        let local_provider = agentprism_openrouter::local_openrouter_provider(local_transport)
            .expect("local OpenRouter provider");
        let local = LocalModels::builder()
            .provider(local_provider)
            .build()
            .expect("LocalModels");
        let local_model = local
            .image_model(&ModelRef::new(
                "openrouter",
                "google/gemini-2.5-flash-image",
            ))
            .expect("local image model");
        let mut local_options = ImageGenerationOptions::default();
        local_options.auth.api_key = Some(SecretString::new(SECRET));
        let local_result = futures_executor::block_on(local.generate_images(
            local_model,
            ImageGenerationContext::default(),
            local_options,
            CancellationToken::new(),
        ));
        let local_message = local_result.error_message.expect("local terminal error");
        assert!(
            !local_message.contains(SECRET),
            "local leaked secret: {local_message}"
        );
        assert!(local_message.contains("[REDACTED]"));
    }
}

fn configured_models_with_transport<T>(transport: Arc<T>) -> Models
where
    T: HttpTransport,
{
    let transport: Arc<dyn HttpTransport> = transport;
    Models::builder()
        .provider(
            agentprism_openrouter::openrouter_provider(transport)
                .expect("OpenRouter provider registration"),
        )
        .build()
        .expect("Models registration")
}

struct TelemetryProbe {
    observed: Arc<AtomicUsize>,
    expected: TelemetryContextHandle,
}

impl ImagesApi for TelemetryProbe {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        let observed = Arc::clone(&self.observed);
        let expected = self.expected.clone();
        Box::pin(async move {
            if request
                .request_options
                .telemetry_context
                .as_ref()
                .is_some_and(|actual| actual.ptr_eq(&expected))
            {
                observed.fetch_add(1, Ordering::SeqCst);
            }
            AssistantImages::empty(&request.model)
        })
    }
}

impl LocalImagesApi for TelemetryProbe {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages> {
        let observed = Arc::clone(&self.observed);
        let expected = self.expected.clone();
        Box::pin(async move {
            if request
                .request_options
                .telemetry_context
                .as_ref()
                .is_some_and(|actual| actual.ptr_eq(&expected))
            {
                observed.fetch_add(1, Ordering::SeqCst);
            }
            AssistantImages::empty(&request.model)
        })
    }
}

fn synthetic_model(provider: &str, model: &str) -> ImageModelDescriptor {
    ImageModelDescriptor {
        model_ref: ModelRef::new(provider, model),
        display_name: model.into(),
        api: ApiId::new(OPENROUTER_IMAGES_API_ID),
        base_url: Url::parse("https://example.invalid/v1").expect("URL"),
        input: vec![ImageModality::Text],
        output: vec![ImageModality::Image],
        pricing: zero_pricing(),
        headers: HeaderMapSpec::new(),
    }
}

fn synthetic_provider(id: &str, model: &str, api: Arc<dyn ImagesApi>) -> ProviderRegistration {
    ProviderRegistration::builder(id)
        .image_models(vec![synthetic_model(id, model)])
        .image_api(OPENROUTER_IMAGES_API_ID, api)
        .build()
        .expect("synthetic image provider")
}

fn local_synthetic_provider(
    id: &str,
    model: &str,
    api: Rc<dyn LocalImagesApi>,
) -> LocalProviderRegistration {
    LocalProviderRegistration::builder(id)
        .image_models(vec![synthetic_model(id, model)])
        .image_api(OPENROUTER_IMAGES_API_ID, api)
        .build()
        .expect("synthetic local image provider")
}

#[test]
fn image_telemetry_context_survives_direct_and_models_dispatch_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/telemetry-options.test.ts "survives direct
    // and ImagesModels image dispatch". Exercise both architecture §9.2
    // trait families at both dispatch boundaries.
    let expected = TelemetryContextHandle::new(String::from("image-trace"));
    let observed = Arc::new(AtomicUsize::new(0));

    let send_direct = TelemetryProbe {
        observed: Arc::clone(&observed),
        expected: expected.clone(),
    };
    let send_direct_result = futures_executor::block_on(ImagesApi::generate(
        &send_direct,
        direct_send_image_request(
            synthetic_model("send-direct", "probe"),
            Some(expected.clone()),
        ),
        CancellationToken::new(),
    ));
    assert_eq!(
        send_direct_result.stop_reason,
        ImageGenerationStopReason::Stop
    );

    let provider = synthetic_provider(
        "send-models",
        "probe",
        Arc::new(TelemetryProbe {
            observed: Arc::clone(&observed),
            expected: expected.clone(),
        }),
    );
    let models = Models::builder()
        .provider(provider)
        .build()
        .expect("Models");
    let result = futures_executor::block_on(models.generate_images(
        synthetic_model("send-models", "probe"),
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            request: agentprism_ai::ApiRequestOptions {
                telemetry_context: Some(expected.clone()),
                ..agentprism_ai::ApiRequestOptions::default()
            },
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Stop);

    let local_direct = TelemetryProbe {
        observed: Arc::clone(&observed),
        expected: expected.clone(),
    };
    let local_direct_result = futures_executor::block_on(LocalImagesApi::generate(
        &local_direct,
        direct_local_image_request(
            synthetic_model("local-direct", "probe"),
            Some(expected.clone()),
        ),
        CancellationToken::new(),
    ));
    assert_eq!(
        local_direct_result.stop_reason,
        ImageGenerationStopReason::Stop
    );

    let local = LocalModels::builder()
        .provider(local_synthetic_provider(
            "local-models",
            "probe",
            Rc::new(TelemetryProbe {
                observed: Arc::clone(&observed),
                expected: expected.clone(),
            }),
        ))
        .build()
        .expect("LocalModels");
    let local_result = futures_executor::block_on(local.generate_images(
        synthetic_model("local-models", "probe"),
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            request: agentprism_ai::ApiRequestOptions {
                telemetry_context: Some(expected),
                ..agentprism_ai::ApiRequestOptions::default()
            },
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(observed.load(Ordering::SeqCst), 4);
}

#[test]
fn models_image_registers_providers_and_reads_models_synchronously_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/images-models.test.ts "registers providers
    // and reads models synchronously", including registration order, scoped
    // reads, missing lookup, replacement, and deletion.
    let no_op: Arc<dyn ImagesApi> = Arc::new(NoopImageApi);
    let models = Models::builder()
        .provider(synthetic_provider("first", "one", Arc::clone(&no_op)))
        .provider(synthetic_provider("second", "two", Arc::clone(&no_op)))
        .build()
        .expect("Models");
    assert_eq!(
        models
            .providers()
            .iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(models.provider(&"first".into()).is_some());
    assert_eq!(
        models
            .image_models()
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(
        models
            .image_models_for(&"first".into())
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["one"]
    );
    assert!(
        models
            .image_model(&ModelRef::new("second", "missing"))
            .is_none()
    );
    models
        .set_provider(synthetic_provider(
            "first",
            "replacement",
            Arc::clone(&no_op),
        ))
        .expect("replace provider");
    assert_eq!(
        models
            .image_models()
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["replacement", "two"]
    );
    assert!(
        models
            .remove_provider(&agentprism_ai::ProviderId::new("first"))
            .is_some()
    );
    assert!(
        models
            .image_model(&ModelRef::new("first", "replacement"))
            .is_none()
    );

    let local_no_op: Rc<dyn LocalImagesApi> = Rc::new(NoopImageApi);
    let local = LocalModels::builder()
        .provider(local_synthetic_provider(
            "local-first",
            "one",
            Rc::clone(&local_no_op),
        ))
        .provider(local_synthetic_provider(
            "local-second",
            "two",
            Rc::clone(&local_no_op),
        ))
        .build()
        .expect("LocalModels");
    assert_eq!(
        local
            .providers()
            .iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        ["local-first", "local-second"]
    );
    assert_eq!(
        local
            .image_models()
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(
        local
            .image_models_for(&"local-first".into())
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["one"]
    );
    assert!(
        local
            .image_model(&ModelRef::new("local-second", "missing"))
            .is_none()
    );
    local
        .set_provider(local_synthetic_provider(
            "local-first",
            "replacement",
            Rc::clone(&local_no_op),
        ))
        .expect("replace local provider");
    assert_eq!(
        local
            .image_models()
            .iter()
            .map(|model| model.model_ref.model.as_str())
            .collect::<Vec<_>>(),
        ["replacement", "two"]
    );
    assert!(
        local
            .remove_provider(&agentprism_ai::ProviderId::new("local-first"))
            .is_some()
    );
}

struct UnconfiguredAuth;

impl AuthResolver for UnconfiguredAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(None) })
    }
}

impl LocalAuthResolver for UnconfiguredAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(None) })
    }
}

struct PendingAuth {
    polled: Arc<AtomicBool>,
}

impl AuthResolver for PendingAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let polled = Arc::clone(&self.polled);
        Box::pin(futures_util::future::poll_fn(move |_| {
            polled.store(true, Ordering::SeqCst);
            std::task::Poll::Pending
        }))
    }
}

impl LocalAuthResolver for PendingAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let polled = Arc::clone(&self.polled);
        Box::pin(futures_util::future::poll_fn(move |_| {
            polled.store(true, Ordering::SeqCst);
            std::task::Poll::Pending
        }))
    }
}

struct CancelBeforeAuthReturn;

impl AuthResolver for CancelBeforeAuthReturn {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        cancellation.cancel();
        Box::pin(async { Ok(None) })
    }
}

impl LocalAuthResolver for CancelBeforeAuthReturn {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        cancellation.cancel();
        Box::pin(async { Ok(None) })
    }
}

async fn cancel_after_auth_is_pending(polled: Arc<AtomicBool>, cancellation: CancellationToken) {
    futures_util::future::poll_fn(move |context| {
        if polled.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
    cancellation.cancel();
}

#[test]
fn models_image_pending_auth_cancellation_send_and_local_pi_exact() {
    // Architecture v2 part 2 §9.5 portable cancellation boundary. Pi basis:
    // packages/ai/src/images-models.ts catches resolveProviderAuth rejection
    // and unconditionally returns stopReason "error" before provider dispatch.
    let send_polled = Arc::new(AtomicBool::new(false));
    let send_called = Arc::new(AtomicBool::new(false));
    let send_model = ImageModelDescriptor {
        api: ApiId::new("pending-images"),
        ..synthetic_model("pending-auth", "probe")
    };
    let send = Models::builder()
        .provider(
            ProviderRegistration::builder("pending-auth")
                .auth(Arc::new(PendingAuth {
                    polled: Arc::clone(&send_polled),
                }))
                .image_models(vec![send_model.clone()])
                .image_api(
                    "pending-images",
                    Arc::new(DispatchProbe {
                        called: Arc::clone(&send_called),
                    }),
                )
                .build()
                .expect("send pending-auth provider"),
        )
        .build()
        .expect("send pending-auth Models");
    let cancellation = CancellationToken::new();
    let send_result = futures_executor::block_on(async {
        futures_util::future::join(
            send.generate_images(
                send_model,
                ImageGenerationContext::default(),
                ImageGenerationOptions::default(),
                cancellation.clone(),
            ),
            cancel_after_auth_is_pending(Arc::clone(&send_polled), cancellation),
        )
        .await
        .0
    });
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(
        send_result.error_message.as_deref(),
        Some("Request aborted")
    );
    assert!(!send_called.load(Ordering::SeqCst));

    let local_polled = Arc::new(AtomicBool::new(false));
    let local_called = Arc::new(AtomicBool::new(false));
    let local_model = ImageModelDescriptor {
        api: ApiId::new("pending-images"),
        ..synthetic_model("local-pending-auth", "probe")
    };
    let local = LocalModels::builder()
        .provider(
            LocalProviderRegistration::builder("local-pending-auth")
                .auth(Rc::new(PendingAuth {
                    polled: Arc::clone(&local_polled),
                }))
                .image_models(vec![local_model.clone()])
                .image_api(
                    "pending-images",
                    Rc::new(DispatchProbe {
                        called: Arc::clone(&local_called),
                    }),
                )
                .build()
                .expect("local pending-auth provider"),
        )
        .build()
        .expect("local pending-auth Models");
    let cancellation = CancellationToken::new();
    let local_result = futures_executor::block_on(async {
        futures_util::future::join(
            local.generate_images(
                local_model,
                ImageGenerationContext::default(),
                ImageGenerationOptions::default(),
                cancellation.clone(),
            ),
            cancel_after_auth_is_pending(Arc::clone(&local_polled), cancellation),
        )
        .await
        .0
    });
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(
        local_result.error_message.as_deref(),
        Some("Request aborted")
    );
    assert!(!local_called.load(Ordering::SeqCst));
}

#[test]
fn models_image_auth_resolver_triggered_cancellation_send_and_local_pi_exact() {
    // Architecture v2 part 2 §9.5 pre-dispatch cancellation boundary. Pi
    // basis: packages/ai/src/images-models.ts catches auth-resolution failures
    // as stopReason "error"; only cancellation in the provider adapter is
    // represented as "aborted".
    let send_called = Arc::new(AtomicBool::new(false));
    let send_model = ImageModelDescriptor {
        api: ApiId::new("cancel-after-auth-images"),
        ..synthetic_model("cancel-after-auth", "probe")
    };
    let send = Models::builder()
        .provider(
            ProviderRegistration::builder("cancel-after-auth")
                .auth(Arc::new(CancelBeforeAuthReturn))
                .image_models(vec![send_model.clone()])
                .image_api(
                    "cancel-after-auth-images",
                    Arc::new(DispatchProbe {
                        called: Arc::clone(&send_called),
                    }),
                )
                .build()
                .expect("send post-auth cancellation provider"),
        )
        .build()
        .expect("send post-auth cancellation Models");
    let send_result = futures_executor::block_on(send.generate_images(
        send_model,
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Error);
    assert!(!send_called.load(Ordering::SeqCst));

    let local_called = Arc::new(AtomicBool::new(false));
    let local_model = ImageModelDescriptor {
        api: ApiId::new("cancel-after-auth-images"),
        ..synthetic_model("local-cancel-after-auth", "probe")
    };
    let local = LocalModels::builder()
        .provider(
            LocalProviderRegistration::builder("local-cancel-after-auth")
                .auth(Rc::new(CancelBeforeAuthReturn))
                .image_models(vec![local_model.clone()])
                .image_api(
                    "cancel-after-auth-images",
                    Rc::new(DispatchProbe {
                        called: Arc::clone(&local_called),
                    }),
                )
                .build()
                .expect("local post-auth cancellation provider"),
        )
        .build()
        .expect("local post-auth cancellation Models");
    let local_result = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Error);
    assert!(!local_called.load(Ordering::SeqCst));
}

struct DispatchProbe {
    called: Arc<AtomicBool>,
}

struct ProviderEnvironmentAuth;

fn resolved_image_auth(overrides: AuthResolutionOverrides) -> ResolvedAuth {
    ResolvedAuth {
        api_key: overrides
            .api_key
            .or_else(|| Some(SecretString::new("provider-image-key"))),
        headers: HeaderMap::new(),
        transport_headers: HeaderMap::new(),
        environment: BTreeMap::from([
            ("PROVIDER_ONLY".to_owned(), "provider".to_owned()),
            ("SHARED".to_owned(), "provider".to_owned()),
        ]),
        base_url: Some(Url::parse("https://resolved.example/v1").expect("resolved URL")),
        source: AuthSource::new("provider image auth"),
    }
}

impl AuthResolver for ProviderEnvironmentAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move { Ok(Some(resolved_image_auth(request.overrides))) })
    }
}

impl LocalAuthResolver for ProviderEnvironmentAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move { Ok(Some(resolved_image_auth(request.overrides))) })
    }
}

struct EnvironmentProbe {
    calls: Arc<AtomicUsize>,
}

fn assert_resolved_image_environment(
    model: &ImageModelDescriptor,
    environment: &BTreeMap<String, String>,
    api_key: Option<&SecretString>,
    endpoint: &Url,
) {
    assert_eq!(model.api.as_str(), "environment-images");
    assert_eq!(
        environment.get("PROVIDER_ONLY").map(String::as_str),
        Some("provider")
    );
    assert_eq!(
        environment.get("REQUEST_ONLY").map(String::as_str),
        Some("request")
    );
    assert_eq!(
        environment.get("SHARED").map(String::as_str),
        Some("request")
    );
    assert_eq!(
        api_key.map(SecretString::expose_secret),
        Some("explicit-image-key")
    );
    assert_eq!(endpoint.as_str(), "https://resolved.example/v1");
}

impl ImagesApi for EnvironmentProbe {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            assert_resolved_image_environment(
                &request.model,
                &request.environment,
                request.api_key.as_ref(),
                &request.endpoint,
            );
            calls.fetch_add(1, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

impl LocalImagesApi for EnvironmentProbe {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            assert_resolved_image_environment(
                &request.model,
                &request.environment,
                request.api_key.as_ref(),
                &request.endpoint,
            );
            calls.fetch_add(1, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

#[test]
fn models_image_resolves_auth_and_merges_environment_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/images-models.test.ts "resolves auth through
    // the provider ... explicit options win" and "merges provider-resolved
    // env into image options" scenarios.
    let model = ImageModelDescriptor {
        api: ApiId::new("environment-images"),
        ..synthetic_model("environment-provider", "probe")
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let send_provider = ProviderRegistration::builder("environment-provider")
        .auth(Arc::new(ProviderEnvironmentAuth))
        .image_models(vec![model.clone()])
        .image_api(
            "environment-images",
            Arc::new(EnvironmentProbe {
                calls: Arc::clone(&calls),
            }),
        )
        .build()
        .expect("send environment provider");
    let send = Models::builder()
        .provider(send_provider)
        .build()
        .expect("Models");
    let provider_auth = futures_executor::block_on(send.resolve_auth(
        "environment-provider".into(),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .expect("provider auth lookup")
    .expect("configured provider auth");
    assert_eq!(
        provider_auth
            .api_key
            .as_ref()
            .map(SecretString::expose_secret),
        Some("provider-image-key")
    );
    let explicit_auth = futures_executor::block_on(send.resolve_auth(
        "environment-provider".into(),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("explicit-image-key")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .expect("explicit auth lookup")
    .expect("configured explicit auth");
    assert_eq!(
        explicit_auth
            .api_key
            .as_ref()
            .map(SecretString::expose_secret),
        Some("explicit-image-key")
    );
    let mut auth = AuthResolutionOverrides {
        api_key: Some(SecretString::new("explicit-image-key")),
        ..AuthResolutionOverrides::default()
    };
    auth.environment
        .insert("REQUEST_ONLY".into(), "request".into());
    auth.environment.insert("SHARED".into(), "request".into());
    let send_result = futures_executor::block_on(send.generate_images(
        model.clone(),
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            auth: auth.clone(),
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);

    let local_provider = LocalProviderRegistration::builder("environment-provider")
        .auth(Rc::new(ProviderEnvironmentAuth))
        .image_models(vec![model.clone()])
        .image_api(
            "environment-images",
            Rc::new(EnvironmentProbe {
                calls: Arc::clone(&calls),
            }),
        )
        .build()
        .expect("local environment provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("LocalModels");
    let provider_auth = futures_executor::block_on(local.resolve_auth(
        "environment-provider".into(),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .expect("local provider auth lookup")
    .expect("configured local provider auth");
    assert_eq!(
        provider_auth
            .api_key
            .as_ref()
            .map(SecretString::expose_secret),
        Some("provider-image-key")
    );
    let explicit_auth = futures_executor::block_on(local.resolve_auth(
        "environment-provider".into(),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("explicit-image-key")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .expect("local explicit auth lookup")
    .expect("configured local explicit auth");
    assert_eq!(
        explicit_auth
            .api_key
            .as_ref()
            .map(SecretString::expose_secret),
        Some("explicit-image-key")
    );
    let local_result = futures_executor::block_on(local.generate_images(
        model,
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            auth,
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

impl ImagesApi for DispatchProbe {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        let called = Arc::clone(&self.called);
        Box::pin(async move {
            assert!(request.api_key.is_none());
            assert_eq!(request.model.api.as_str(), "custom-images");
            called.store(true, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

impl LocalImagesApi for DispatchProbe {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages> {
        let called = Arc::clone(&self.called);
        Box::pin(async move {
            assert!(request.api_key.is_none());
            assert_eq!(request.model.api.as_str(), "custom-images");
            called.store(true, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

#[test]
fn models_image_unknown_provider_and_unconfigured_auth_are_in_band_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/images-models.test.ts error and unconfigured auth scenarios.
    let unknown = futures_executor::block_on(Models::default().generate_images(
        ImageModelDescriptor {
            api: ApiId::new("custom-images"),
            ..synthetic_model("missing", "model")
        },
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(unknown.stop_reason, ImageGenerationStopReason::Error);
    assert_eq!(unknown.api.as_str(), "custom-images");
    assert!(
        unknown
            .error_message
            .expect("unknown error")
            .contains("Unknown provider")
    );

    let called = Arc::new(AtomicBool::new(false));
    let model = ImageModelDescriptor {
        api: ApiId::new("custom-images"),
        ..synthetic_model("unconfigured", "probe")
    };
    let provider = ProviderRegistration::builder("unconfigured")
        .auth(Arc::new(UnconfiguredAuth))
        .image_models(vec![model.clone()])
        .image_api(
            "custom-images",
            Arc::new(DispatchProbe {
                called: Arc::clone(&called),
            }),
        )
        .build()
        .expect("unconfigured synthetic provider");
    let models = Models::builder()
        .provider(provider)
        .build()
        .expect("Models");
    let unconfigured = futures_executor::block_on(models.generate_images(
        model,
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(unconfigured.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(unconfigured.api.as_str(), "custom-images");
    assert!(called.load(Ordering::SeqCst));

    let local_unknown = futures_executor::block_on(LocalModels::default().generate_images(
        ImageModelDescriptor {
            api: ApiId::new("custom-images"),
            ..synthetic_model("local-missing", "model")
        },
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(local_unknown.stop_reason, ImageGenerationStopReason::Error);
    assert!(
        local_unknown
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("Unknown provider"))
    );

    let local_called = Arc::new(AtomicBool::new(false));
    let local_model = ImageModelDescriptor {
        api: ApiId::new("custom-images"),
        ..synthetic_model("local-unconfigured", "probe")
    };
    let local_provider = LocalProviderRegistration::builder("local-unconfigured")
        .auth(Rc::new(UnconfiguredAuth))
        .image_models(vec![local_model.clone()])
        .image_api(
            "custom-images",
            Rc::new(DispatchProbe {
                called: Arc::clone(&local_called),
            }),
        )
        .build()
        .expect("unconfigured local provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .build()
        .expect("LocalModels");
    let local_unconfigured = futures_executor::block_on(local.generate_images(
        local_model,
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(
        local_unconfigured.stop_reason,
        ImageGenerationStopReason::Stop
    );
    assert!(local_called.load(Ordering::SeqCst));
}

#[test]
fn openrouter_images_missing_api_key_is_in_band() {
    // Pi basis: packages/ai/test/openrouter-images.test.ts missing API-key case.
    let transport = Arc::new(FixtureTransport::default());
    let models = configured_models(Arc::clone(&transport));
    let model = configured_image_model(&models);
    let result = futures_executor::block_on(models.generate_images(
        model,
        ImageGenerationContext::default(),
        ImageGenerationOptions::default(),
        CancellationToken::new(),
    ));
    assert_eq!(result.stop_reason, ImageGenerationStopReason::Error);
    assert!(
        result
            .error_message
            .expect("auth error")
            .contains("No API key")
    );
    assert!(lock(&transport.requests).is_empty());
}

struct DynamicImageSource {
    provider: &'static str,
    result: Result<&'static str, &'static str>,
    calls: Arc<AtomicUsize>,
}

struct GatedImageSource {
    provider: &'static str,
    results: &'static [Result<&'static str, &'static str>],
    releases: Arc<Vec<AtomicBool>>,
    calls: Arc<AtomicUsize>,
}

impl GatedImageSource {
    async fn fetch_result(&self) -> Result<Vec<ImageModelDescriptor>, ImageCatalogError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let releases = Arc::clone(&self.releases);
        futures_util::future::poll_fn(move |_| {
            if releases
                .get(index)
                .is_some_and(|release| release.load(Ordering::SeqCst))
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        self.results
            .get(index)
            .copied()
            .unwrap_or(Err("unexpected image refresh"))
            .map(|model| vec![synthetic_model(self.provider, model)])
            .map_err(ImageCatalogError::new)
    }
}

impl ImageModelCatalogSource for GatedImageSource {
    fn fetch(
        &self,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>> {
        Box::pin(self.fetch_result())
    }
}

impl LocalImageModelCatalogSource for GatedImageSource {
    fn fetch(
        &self,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>> {
        Box::pin(self.fetch_result())
    }
}

fn send_dynamic_image_provider(
    id: &str,
    source: Arc<dyn ImageModelCatalogSource>,
) -> ProviderRegistration {
    ProviderRegistration::builder(id)
        .image_model_source(source)
        .image_api(OPENROUTER_IMAGES_API_ID, Arc::new(NoopImageApi))
        .build()
        .expect("send dynamic image provider")
}

fn local_dynamic_image_provider(
    id: &str,
    source: Rc<dyn LocalImageModelCatalogSource>,
) -> LocalProviderRegistration {
    LocalProviderRegistration::builder(id)
        .image_model_source(source)
        .image_api(OPENROUTER_IMAGES_API_ID, Rc::new(NoopImageApi))
        .build()
        .expect("local dynamic image provider")
}

#[test]
fn models_image_refresh_replacement_and_cleanup_are_generation_scoped_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.7 `catalog_superseded_refresh_cannot_publish`
    // applied to the image catalog, plus Pi's createImagesProvider in-flight
    // dedupe invariant from packages/ai/src/images-models.ts. A registration
    // replacement gets a distinct refresh generation, and a late waiter may
    // clean up only the exact shared task it awaited.
    let mut task_context = TaskContext::from_waker(futures_util::task::noop_waker_ref());

    let old_releases = Arc::new(vec![AtomicBool::new(false)]);
    let old_calls = Arc::new(AtomicUsize::new(0));
    let send = Models::builder()
        .provider(send_dynamic_image_provider(
            "generation",
            Arc::new(GatedImageSource {
                provider: "generation",
                results: &[Ok("old")],
                releases: Arc::clone(&old_releases),
                calls: Arc::clone(&old_calls),
            }),
        ))
        .build()
        .expect("send generation Models");
    let mut old_refresh =
        send.refresh_image_models(Some("generation".into()), CancellationToken::new());
    assert!(matches!(
        old_refresh.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    let new_calls = Arc::new(AtomicUsize::new(0));
    send.set_provider(send_dynamic_image_provider(
        "generation",
        Arc::new(GatedImageSource {
            provider: "generation",
            results: &[Ok("new")],
            releases: Arc::new(vec![AtomicBool::new(true)]),
            calls: Arc::clone(&new_calls),
        }),
    ))
    .expect("replace send generation provider");
    futures_executor::block_on(
        send.refresh_image_models(Some("generation".into()), CancellationToken::new()),
    )
    .expect("new send registration refresh");
    assert!(
        send.image_model(&ModelRef::new("generation", "new"))
            .is_some()
    );
    old_releases[0].store(true, Ordering::SeqCst);
    match old_refresh.as_mut().poll(&mut task_context) {
        Poll::Ready(result) => result.expect("old send refresh completes for its caller"),
        Poll::Pending => panic!("released old send refresh stayed pending"),
    }
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        send.image_model(&ModelRef::new("generation", "new"))
            .expect("new send snapshot remains published")
            .model_ref
            .model
            .as_str(),
        "new"
    );
    assert!(
        send.image_model(&ModelRef::new("generation", "old"))
            .is_none()
    );

    let cleanup_releases = Arc::new(vec![
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ]);
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let cleanup = Models::builder()
        .provider(send_dynamic_image_provider(
            "cleanup",
            Arc::new(GatedImageSource {
                provider: "cleanup",
                results: &[Err("first failed"), Ok("second"), Ok("unexpected-third")],
                releases: Arc::clone(&cleanup_releases),
                calls: Arc::clone(&cleanup_calls),
            }),
        ))
        .build()
        .expect("send cleanup Models");
    let mut first = cleanup.refresh_image_models(Some("cleanup".into()), CancellationToken::new());
    let mut late = cleanup.refresh_image_models(Some("cleanup".into()), CancellationToken::new());
    assert!(matches!(
        first.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert!(matches!(
        late.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    cleanup_releases[0].store(true, Ordering::SeqCst);
    assert!(matches!(
        first.as_mut().poll(&mut task_context),
        Poll::Ready(Err(_))
    ));
    let mut second = cleanup.refresh_image_models(Some("cleanup".into()), CancellationToken::new());
    assert!(matches!(
        second.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert!(matches!(
        late.as_mut().poll(&mut task_context),
        Poll::Ready(Err(_))
    ));
    let mut second_waiter =
        cleanup.refresh_image_models(Some("cleanup".into()), CancellationToken::new());
    assert!(matches!(
        second_waiter.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);
    cleanup_releases[1].store(true, Ordering::SeqCst);
    assert!(matches!(
        second.as_mut().poll(&mut task_context),
        Poll::Ready(Ok(_))
    ));
    assert!(matches!(
        second_waiter.as_mut().poll(&mut task_context),
        Poll::Ready(Ok(_))
    ));
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);

    let local_old_releases = Arc::new(vec![AtomicBool::new(false)]);
    let local_old_calls = Arc::new(AtomicUsize::new(0));
    let local = LocalModels::builder()
        .provider(local_dynamic_image_provider(
            "local-generation",
            Rc::new(GatedImageSource {
                provider: "local-generation",
                results: &[Ok("old")],
                releases: Arc::clone(&local_old_releases),
                calls: Arc::clone(&local_old_calls),
            }),
        ))
        .build()
        .expect("local generation Models");
    let mut local_old_refresh =
        local.refresh_image_models(Some("local-generation".into()), CancellationToken::new());
    assert!(matches!(
        local_old_refresh.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    let local_new_calls = Arc::new(AtomicUsize::new(0));
    local
        .set_provider(local_dynamic_image_provider(
            "local-generation",
            Rc::new(GatedImageSource {
                provider: "local-generation",
                results: &[Ok("new")],
                releases: Arc::new(vec![AtomicBool::new(true)]),
                calls: Arc::clone(&local_new_calls),
            }),
        ))
        .expect("replace local generation provider");
    futures_executor::block_on(
        local.refresh_image_models(Some("local-generation".into()), CancellationToken::new()),
    )
    .expect("new local registration refresh");
    assert!(
        local
            .image_model(&ModelRef::new("local-generation", "new"))
            .is_some()
    );
    local_old_releases[0].store(true, Ordering::SeqCst);
    assert!(matches!(
        local_old_refresh.as_mut().poll(&mut task_context),
        Poll::Ready(Ok(_))
    ));
    assert_eq!(local_new_calls.load(Ordering::SeqCst), 1);
    assert!(
        local
            .image_model(&ModelRef::new("local-generation", "new"))
            .is_some()
    );
    assert!(
        local
            .image_model(&ModelRef::new("local-generation", "old"))
            .is_none()
    );

    let local_cleanup_releases = Arc::new(vec![
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ]);
    let local_cleanup_calls = Arc::new(AtomicUsize::new(0));
    let local_cleanup = LocalModels::builder()
        .provider(local_dynamic_image_provider(
            "local-cleanup",
            Rc::new(GatedImageSource {
                provider: "local-cleanup",
                results: &[Err("first failed"), Ok("second"), Ok("unexpected-third")],
                releases: Arc::clone(&local_cleanup_releases),
                calls: Arc::clone(&local_cleanup_calls),
            }),
        ))
        .build()
        .expect("local cleanup Models");
    let mut first =
        local_cleanup.refresh_image_models(Some("local-cleanup".into()), CancellationToken::new());
    let mut late =
        local_cleanup.refresh_image_models(Some("local-cleanup".into()), CancellationToken::new());
    assert!(matches!(
        first.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert!(matches!(
        late.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    local_cleanup_releases[0].store(true, Ordering::SeqCst);
    assert!(matches!(
        first.as_mut().poll(&mut task_context),
        Poll::Ready(Err(_))
    ));
    let mut second =
        local_cleanup.refresh_image_models(Some("local-cleanup".into()), CancellationToken::new());
    assert!(matches!(
        second.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert!(matches!(
        late.as_mut().poll(&mut task_context),
        Poll::Ready(Err(_))
    ));
    let mut second_waiter =
        local_cleanup.refresh_image_models(Some("local-cleanup".into()), CancellationToken::new());
    assert!(matches!(
        second_waiter.as_mut().poll(&mut task_context),
        Poll::Pending
    ));
    assert_eq!(local_cleanup_calls.load(Ordering::SeqCst), 2);
    local_cleanup_releases[1].store(true, Ordering::SeqCst);
    assert!(matches!(
        second.as_mut().poll(&mut task_context),
        Poll::Ready(Ok(_))
    ));
    assert!(matches!(
        second_waiter.as_mut().poll(&mut task_context),
        Poll::Ready(Ok(_))
    ));
    assert_eq!(local_cleanup_calls.load(Ordering::SeqCst), 2);
}

impl ImageModelCatalogSource for DynamicImageSource {
    fn fetch(
        &self,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>> {
        let calls = Arc::clone(&self.calls);
        let provider = self.provider;
        let result = self.result;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let mut yielded = false;
            futures_util::future::poll_fn(move |context| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            result
                .map(|model| vec![synthetic_model(provider, model)])
                .map_err(ImageCatalogError::new)
        })
    }
}

impl LocalImageModelCatalogSource for DynamicImageSource {
    fn fetch(
        &self,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>> {
        let calls = Arc::clone(&self.calls);
        let provider = self.provider;
        let result = self.result;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let mut yielded = false;
            futures_util::future::poll_fn(move |context| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            result
                .map(|model| vec![synthetic_model(provider, model)])
                .map_err(ImageCatalogError::new)
        })
    }
}

struct ImageMetadataProbe {
    expected: BTreeMap<String, Value>,
    calls: Arc<AtomicUsize>,
}

impl ImagesApi for ImageMetadataProbe {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        let expected = self.expected.clone();
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            assert_eq!(request.metadata, expected);
            calls.fetch_add(1, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

impl LocalImagesApi for ImageMetadataProbe {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages> {
        let expected = self.expected.clone();
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            assert_eq!(request.metadata, expected);
            calls.fetch_add(1, Ordering::SeqCst);
            AssistantImages::empty(&request.model)
        })
    }
}

#[test]
fn models_image_metadata_reaches_provider_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/types.ts ImagesOptions.metadata and
    // packages/ai/src/images-models.ts generateImages option dispatch.
    let metadata = BTreeMap::from([
        ("request_id".into(), serde_json::json!("image-request-1")),
        (
            "nested".into(),
            serde_json::json!({"quality": "high", "seed": 7}),
        ),
    ]);
    let model = ImageModelDescriptor {
        api: ApiId::new("metadata-images"),
        ..synthetic_model("metadata-provider", "metadata-model")
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let send = Models::builder()
        .provider(
            ProviderRegistration::builder("metadata-provider")
                .image_models(vec![model.clone()])
                .image_api(
                    "metadata-images",
                    Arc::new(ImageMetadataProbe {
                        expected: metadata.clone(),
                        calls: Arc::clone(&calls),
                    }),
                )
                .build()
                .expect("send metadata provider"),
        )
        .build()
        .expect("send metadata Models");
    let send_result = futures_executor::block_on(send.generate_images(
        model.clone(),
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            metadata: metadata.clone(),
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(send_result.stop_reason, ImageGenerationStopReason::Stop);

    let local = LocalModels::builder()
        .provider(
            LocalProviderRegistration::builder("metadata-provider")
                .image_models(vec![model.clone()])
                .image_api(
                    "metadata-images",
                    Rc::new(ImageMetadataProbe {
                        expected: metadata.clone(),
                        calls: Arc::clone(&calls),
                    }),
                )
                .build()
                .expect("local metadata provider"),
        )
        .build()
        .expect("local metadata Models");
    let local_result = futures_executor::block_on(local.generate_images(
        model,
        ImageGenerationContext::default(),
        ImageGenerationOptions {
            metadata,
            ..ImageGenerationOptions::default()
        },
        CancellationToken::new(),
    ));
    assert_eq!(local_result.stop_reason, ImageGenerationStopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[derive(Default)]
struct NoopImageApi;

impl ImagesApi for NoopImageApi {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        Box::pin(async move { AssistantImages::empty(&request.model) })
    }
}

impl LocalImagesApi for NoopImageApi {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages> {
        Box::pin(async move { AssistantImages::empty(&request.model) })
    }
}

#[test]
fn models_image_catalog_refresh_deduplicates_and_is_best_effort_send_and_local_pi_exact() {
    // Pi basis: packages/ai/test/images-models.test.ts dynamic provider,
    // refresh deduplication, provider-specific failure, and all-provider
    // best-effort scenarios.
    let send_calls = Arc::new(AtomicUsize::new(0));
    let send_provider = ProviderRegistration::builder("dynamic")
        .image_model_source(Arc::new(DynamicImageSource {
            provider: "dynamic",
            result: Ok("fresh"),
            calls: Arc::clone(&send_calls),
        }))
        .image_api(OPENROUTER_IMAGES_API_ID, Arc::new(NoopImageApi))
        .build()
        .expect("dynamic provider");
    let models = Models::builder()
        .provider(send_provider)
        .provider(
            ProviderRegistration::builder("failing")
                .image_model_source(Arc::new(DynamicImageSource {
                    provider: "failing",
                    result: Err("refresh failed"),
                    calls: Arc::new(AtomicUsize::new(0)),
                }))
                .image_api(OPENROUTER_IMAGES_API_ID, Arc::new(NoopImageApi))
                .build()
                .expect("failing provider"),
        )
        .build()
        .expect("Models");
    let (left, right) = futures_executor::block_on(async {
        futures_util::future::join(
            models.refresh_image_models(Some("dynamic".into()), CancellationToken::new()),
            models.refresh_image_models(Some("dynamic".into()), CancellationToken::new()),
        )
        .await
    });
    left.expect("left refresh");
    right.expect("right refresh");
    assert_eq!(send_calls.load(Ordering::SeqCst), 1);
    futures_executor::block_on(models.refresh_image_models(None, CancellationToken::new()))
        .expect("all-provider refresh is best effort");
    assert!(
        models
            .image_models()
            .iter()
            .any(|model| model.model_ref.provider.as_str() == "dynamic")
    );
    let selected_error = futures_executor::block_on(
        models.refresh_image_models(Some("failing".into()), CancellationToken::new()),
    )
    .expect_err("selected failing provider rejects");
    assert_eq!(selected_error.code, "model_source");
    assert_eq!(
        selected_error.kind,
        agentprism_ai::ImageCatalogErrorKind::ModelSource
    );
    assert!(selected_error.message.contains("refresh failed"));

    let local_calls = Arc::new(AtomicUsize::new(0));
    let local_provider = LocalProviderRegistration::builder("local-dynamic")
        .image_model_source(std::rc::Rc::new(DynamicImageSource {
            provider: "local-dynamic",
            result: Ok("fresh-local"),
            calls: Arc::clone(&local_calls),
        }))
        .image_api(OPENROUTER_IMAGES_API_ID, std::rc::Rc::new(NoopImageApi))
        .build()
        .expect("local dynamic provider");
    let local = LocalModels::builder()
        .provider(local_provider)
        .provider(
            LocalProviderRegistration::builder("local-failing")
                .image_model_source(std::rc::Rc::new(DynamicImageSource {
                    provider: "local-failing",
                    result: Err("local refresh failed"),
                    calls: Arc::new(AtomicUsize::new(0)),
                }))
                .image_api(OPENROUTER_IMAGES_API_ID, std::rc::Rc::new(NoopImageApi))
                .build()
                .expect("local failing provider"),
        )
        .build()
        .expect("LocalModels");
    let (left, right) = futures_executor::block_on(async {
        futures_util::future::join(
            local.refresh_image_models(Some("local-dynamic".into()), CancellationToken::new()),
            local.refresh_image_models(Some("local-dynamic".into()), CancellationToken::new()),
        )
        .await
    });
    left.expect("local left");
    right.expect("local right");
    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
    futures_executor::block_on(local.refresh_image_models(None, CancellationToken::new()))
        .expect("local all-provider refresh is best effort");
    assert!(
        local
            .image_models()
            .iter()
            .any(|model| model.model_ref.provider.as_str() == "local-dynamic")
    );
    let local_selected_error = futures_executor::block_on(
        local.refresh_image_models(Some("local-failing".into()), CancellationToken::new()),
    )
    .expect_err("selected local failing provider rejects");
    assert_eq!(local_selected_error.code, "model_source");
    assert_eq!(
        local_selected_error.kind,
        agentprism_ai::ImageCatalogErrorKind::ModelSource
    );
    assert!(
        local_selected_error
            .message
            .contains("local refresh failed")
    );
}
