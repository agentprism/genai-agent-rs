#![allow(
    clippy::result_large_err,
    reason = "test helpers exercise the architecture-specified AiError by value"
)]

use futures_executor::block_on;
use futures_util::StreamExt;
use futures_util::task::noop_waker;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use pi_ai::*;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use static_assertions::assert_not_impl_any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::future::{Future, pending};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, SystemTime};
use url::Url;

const RETRY_BASIS: &str =
    "packages/ai/src/utils/provider-retry.ts:1-125; architecture v2 part 2 §10.3";
const MIDDLEWARE_BASIS: &str = "packages/ai/src/models.ts:188-237,621-696; packages/ai/src/types.ts:112-185; architecture v2 part 2 §10.4";
const BEDROCK_BASIS: &str = "packages/ai/src/api/bedrock-converse-stream.ts:452-522; packages/ai/test/bedrock-custom-headers.test.ts; packages/ai/test/bedrock-response-headers.test.ts";
const SECRET_SENTINEL: &str = "secret-sentinel-do-not-log";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn headers(values: &[(&str, &str)]) -> HeaderMap {
    let mut result = HeaderMap::new();
    for (name, value) in values {
        result.insert(
            HeaderName::try_from(*name).unwrap(),
            HeaderValue::try_from(*value).unwrap(),
        );
    }
    result
}

fn header_spec(values: &[(&str, Option<&str>)]) -> HeaderMapSpec {
    values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.map(str::to_owned)))
        .collect::<BTreeMap<_, _>>()
}

#[derive(Clone, Copy)]
struct FixedJitter(f64);

impl RetryJitter for FixedJitter {
    fn sample(&self, _range: &std::ops::RangeInclusive<f64>) -> f64 {
        self.0
    }
}

fn classifier() -> DefaultRetryClassifier {
    DefaultRetryClassifier::new(FixedJitter(1.0))
}

fn http_failure(status: u16, values: &[(&str, &str)]) -> AttemptFailure {
    AttemptFailure::http(0, status, headers(values), "fixture")
}

fn assert_retry(status: u16) {
    assert!(matches!(
        classifier().classify(&http_failure(status, &[]), &RetryPolicy::default()),
        RetryDecision::RetryAfter(_)
    ));
}

fn assert_no_retry(status: u16) {
    assert_eq!(
        classifier().classify(&http_failure(status, &[]), &RetryPolicy::default()),
        RetryDecision::DoNotRetry
    );
}

#[derive(Clone, Debug)]
enum TransportOutcome {
    Response {
        status: u16,
        headers: HeaderMap,
        body: Vec<u8>,
    },
    Failure(TransportError),
}

#[derive(Default)]
struct FakeHttpTransport {
    outcomes: Mutex<VecDeque<TransportOutcome>>,
    requests: Mutex<Vec<HttpRequest>>,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl FakeHttpTransport {
    fn new(outcomes: impl IntoIterator<Item = TransportOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            log: None,
        }
    }

    fn with_log(
        outcomes: impl IntoIterator<Item = TransportOutcome>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            log: Some(log),
        }
    }

    fn request_count(&self) -> usize {
        lock(&self.requests).len()
    }
}

impl HttpTransport for FakeHttpTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if let Some(log) = &self.log {
                lock(log).push(format!("transport:{}", request.attempt));
            }
            lock(&self.requests).push(request);
            match lock(&self.outcomes)
                .pop_front()
                .expect("fake transport outcome")
            {
                TransportOutcome::Response {
                    status,
                    headers,
                    body,
                } => Ok(HttpResponse::from_bytes(status, headers, body)),
                TransportOutcome::Failure(error) => Err(error),
            }
        })
    }
}

#[derive(Default)]
struct ImmediateSleeper {
    delays: Mutex<Vec<Duration>>,
}

impl RetrySleeper for ImmediateSleeper {
    fn sleep(
        &self,
        duration: Duration,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AttemptFailure>> {
        lock(&self.delays).push(duration);
        Box::pin(async { Ok(()) })
    }
}

struct PendingSleeper;

impl RetrySleeper for PendingSleeper {
    fn sleep(
        &self,
        _duration: Duration,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AttemptFailure>> {
        Box::pin(pending())
    }
}

struct TestHandler {
    api: ApiId,
    body: String,
    encode_count: AtomicUsize,
    events: Vec<AssistantEvent>,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl TestHandler {
    fn new(body: impl AsRef<[u8]>) -> Self {
        Self {
            api: ApiId::new("test-api"),
            body: String::from_utf8(body.as_ref().to_vec()).unwrap(),
            encode_count: AtomicUsize::new(0),
            events: Vec::new(),
            log: None,
        }
    }
}

struct TestApi;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TestWireRequest {
    body: String,
}

impl ApiFamily for TestApi {
    const API_ID: &'static str = "test-api";

    type Compat = ();
    type ModelConfig = ();
    type FullOptions = ();
    type OptionsPatch = ();
    type WireRequest = TestWireRequest;

    fn resolve_compat(
        _effective_base_url: &Url,
        _model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(())
    }

    fn lower_simple(
        _context: SimpleLoweringContext<'_, Self>,
        _simple: &SimpleGenerationOptions,
        _patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        Ok(())
    }

    fn encode(
        _context: EncodeContext<'_, Self>,
        _options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        Ok(TestWireRequest {
            body: String::new(),
        })
    }
}

fn typed_test_model(model: &ModelDescriptor) -> TypedModelDescriptor<TestApi> {
    TypedModelDescriptor {
        common: model.common.clone(),
        config: (),
        extensions: model.extensions.clone(),
    }
}

impl ErasedApiHandler for TestHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        _model: &ModelDescriptor,
        _context: &pi_ai::Context,
        _simple: &SimpleGenerationOptions,
        _patch: Option<&ErasedApiOptionsPatch>,
        _execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        self.encode_count.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderPayload::typed(
            Method::POST,
            typed_test_model(_model),
            TestWireRequest {
                body: self.body.clone(),
            },
            |wire_request| Ok(wire_request.body.as_bytes().to_vec()),
        ))
    }

    fn decode_stream(
        &self,
        _response: ProviderResponseStream,
        _execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        if let Some(log) = &self.log {
            lock(log).push("decode".into());
        }
        AssistantStream::new(futures_util::stream::iter(self.events.clone()))
    }
}

fn model() -> ModelDescriptor {
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("test-provider", "test-model"),
            display_name: "Test model".into(),
            base_url: Url::parse("https://model.example/v1").unwrap(),
            modalities: ModalityCapabilities::default(),
            limits: ModelLimits {
                context_window: 16_384,
                max_output_tokens: 1_024,
            },
            pricing: ModelPricing {
                default: TokenPriceRates::default(),
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: false,
            headers: HeaderMapSpec::new(),
        },
        api: ApiModelConfig::Custom(CustomApiModelConfig {
            api: ApiId::new("test-api"),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

fn resolved_request(
    payload_transforms: Vec<Arc<dyn ErasedPayloadTransform>>,
    response_observers: Vec<Arc<dyn ResponseObserver>>,
    attempt_middleware: Vec<Arc<dyn AttemptMiddleware>>,
    max_retries: u32,
) -> ResolvedApiRequest {
    ResolvedApiRequest {
        model: model(),
        context: pi_ai::Context::new(None),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: pi_ai::ApiRequestOptions::default(),
        endpoint: Url::parse("https://effective.example/v1").unwrap(),
        headers: headers(&[("content-type", "application/json")]),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: ApiId::new("test-api"),
        payload_transforms: Arc::from(payload_transforms),
        response_observers: Arc::from(response_observers),
        attempt_middleware: Arc::from(attempt_middleware),
        retry_policy: RetryPolicy {
            max_retries,
            ..RetryPolicy::default()
        },
        timeout: None,
        retry_classifier: Arc::new(classifier()),
    }
}

fn run_http(
    handler: Arc<TestHandler>,
    transport: Arc<FakeHttpTransport>,
    sleeper: Arc<dyn RetrySleeper>,
    request: ResolvedApiRequest,
) -> Result<AssistantStream, AiError> {
    let api = HttpChatApi::new(handler, transport).with_retry_sleeper(sleeper);
    block_on(api.stream(request, CancellationToken::new()))
}

struct StaticLocalHttpTransport {
    status: u16,
    body: Vec<u8>,
}

impl LocalHttpTransport for StaticLocalHttpTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let status = self.status;
        let body = self.body.clone();
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                status,
                HeaderMap::new(),
                body,
            ))
        })
    }
}

struct PendingErrorBodyTransport;

impl HttpTransport for PendingErrorBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 503,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

struct LocalPendingErrorBodyTransport;

impl LocalHttpTransport for LocalPendingErrorBodyTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async {
            Ok(LocalHttpResponse {
                status: 503,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                body: Box::pin(futures_util::stream::pending()),
            })
        })
    }
}

struct InspectingRetryClassifier(Arc<Mutex<Vec<String>>>);

impl RetryClassifier for InspectingRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, _policy: &RetryPolicy) -> RetryDecision {
        if let AttemptFailure::Http { message, .. } = failure {
            lock(&self.0).push(message.clone());
        }
        RetryDecision::DoNotRetry
    }
}

struct LocalInspectingRetryClassifier(Rc<RefCell<Vec<String>>>);

impl LocalRetryClassifier for LocalInspectingRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, _policy: &RetryPolicy) -> RetryDecision {
        if let AttemptFailure::Http { message, .. } = failure {
            self.0.borrow_mut().push(message.clone());
        }
        RetryDecision::DoNotRetry
    }
}

fn local_resolved_request(
    retry_classifier: Rc<dyn LocalRetryClassifier>,
    max_retries: u32,
) -> LocalResolvedApiRequest {
    let send = resolved_request(Vec::new(), Vec::new(), Vec::new(), max_retries);
    LocalResolvedApiRequest {
        model: send.model,
        context: send.context,
        options: send.options,
        full_options: send.full_options,
        request_options: send.request_options,
        endpoint: send.endpoint,
        headers: send.headers,
        auth_headers: send.auth_headers,
        api_key: send.api_key,
        api: send.api,
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: send.retry_policy,
        timeout: send.timeout,
        retry_classifier,
    }
}

fn assert_debug_redacted(label: &str, value: &impl std::fmt::Debug) {
    let debug = format!("{value:?}");
    assert!(
        !debug.contains(SECRET_SENTINEL),
        "{label} leaked the sentinel through Debug: {debug}"
    );
    assert!(
        debug.to_ascii_lowercase().contains("redacted"),
        "{label} did not mark its secret-bearing fields as redacted: {debug}"
    );
}

#[test]
fn request_debug_redacts_secrets() {
    let _basis =
        "architecture v2 part 1 §3.8; architecture v2 part 2 §10.1 stream_error_sanitizes_secrets";
    let secret_headers = headers(&[
        ("authorization", SECRET_SENTINEL),
        ("x-api-key", SECRET_SENTINEL),
        ("set-cookie", SECRET_SENTINEL),
    ]);
    let secret_url = Url::parse(&format!(
        "https://user:{SECRET_SENTINEL}@provider.example/v1?api_key={SECRET_SENTINEL}"
    ))
    .unwrap();

    let request = HttpRequest {
        method: Method::POST,
        url: secret_url.clone(),
        headers: secret_headers.clone(),
        auth_headers: secret_headers.clone(),
        session_id: Some(SECRET_SENTINEL.into()),
        body: SECRET_SENTINEL.as_bytes().to_vec(),
        timeout: Some(Duration::from_secs(1)),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 2,
    };
    assert_debug_redacted("HttpRequest", &request);

    let response = HttpResponse::from_bytes(
        200,
        secret_headers.clone(),
        SECRET_SENTINEL.as_bytes().to_vec(),
    );
    assert_debug_redacted("HttpResponse", &response);
    let local_response = LocalHttpResponse::from_bytes(
        200,
        secret_headers.clone(),
        SECRET_SENTINEL.as_bytes().to_vec(),
    );
    assert_debug_redacted("LocalHttpResponse", &local_response);
    assert_debug_redacted(
        "ProviderResponseMetadata",
        &ProviderResponseMetadata {
            attempt: 2,
            status: 200,
            headers: secret_headers.clone(),
            request_id: Some("safe-request-id".into()),
        },
    );
    assert_debug_redacted(
        "AttemptFailure",
        &AttemptFailure::http(2, 429, secret_headers.clone(), "sanitized failure"),
    );
    assert_debug_redacted(
        "ResolvedAuth",
        &ResolvedAuth {
            api_key: Some(SecretString::new(SECRET_SENTINEL)),
            headers: secret_headers,
            base_url: Some(secret_url.clone()),
            source: AuthSource::new("fixture"),
        },
    );

    let provider = ProviderDescriptor {
        id: ProviderId::new("test-provider"),
        display_name: "Test provider".into(),
        base_url: Some(secret_url.clone()),
        headers: header_spec(&[("authorization", Some(SECRET_SENTINEL))]),
    };
    assert_debug_redacted("ProviderDescriptor", &provider);

    let mut secret_model = model();
    secret_model.common.base_url = secret_url;
    secret_model.common.headers = header_spec(&[("authorization", Some(SECRET_SENTINEL))]);
    assert_debug_redacted("ModelDescriptor", &secret_model);

    let secret_options = SimpleGenerationOptions {
        session_id: Some(SECRET_SENTINEL.into()),
        headers: header_spec(&[("authorization", Some(SECRET_SENTINEL))]),
        api_options: Some(ErasedApiOptionsPatch {
            api: ApiId::new("test-api"),
            schema_version: 1,
            value: RawValue::from_string(format!("{{\"secret\":\"{SECRET_SENTINEL}\"}}")).unwrap(),
        }),
        ..SimpleGenerationOptions::default()
    };
    assert_debug_redacted("SimpleGenerationOptions", &secret_options);
}

/// Architecture v2 part 2 §10.1 `stream_error_sanitizes_secrets`; real Send
/// and Local HTTP establishment paths. Raw non-2xx response bytes are
/// available to retry classification but never become `AiError` text.
#[test]
fn stream_error_sanitizes_secrets_http_pipeline_send_and_local() {
    let body = format!("provider denied {SECRET_SENTINEL}; body-secret-plain").into_bytes();
    let mut send_request = resolved_request(Vec::new(), Vec::new(), Vec::new(), 0);
    send_request.headers = headers(&[("authorization", SECRET_SENTINEL)]);
    send_request.auth_headers = send_request.headers.clone();
    let send_error = run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
            status: 403,
            headers: HeaderMap::new(),
            body: body.clone(),
        }])),
        Arc::new(ImmediateSleeper::default()),
        send_request,
    )
    .expect_err("Send provider rejection");

    let local_api = LocalHttpChatApi::new(
        Rc::new(RcLocalHandler(Rc::new(Cell::new(0)))),
        Rc::new(StaticLocalHttpTransport { status: 403, body }),
    );
    let mut local_request =
        local_resolved_request(Rc::new(LocalDefaultRetryClassifier::default()), 0);
    local_request.headers = headers(&[("authorization", SECRET_SENTINEL)]);
    local_request.auth_headers = local_request.headers.clone();
    let local_error = block_on(local_api.stream(local_request, CancellationToken::new()))
        .expect_err("local provider rejection");

    for error in [&send_error, &local_error] {
        assert_eq!(error.status, Some(403));
        assert!(
            error
                .message
                .contains("provider rejected request before streaming")
        );
        assert!(!error.message.contains(SECRET_SENTINEL));
        assert!(!error.message.contains("body-secret-plain"));
    }
}

/// Architecture v2 part 2 §2.4/§9.5/§10.1. Non-success bodies are capped at
/// 64 KiB for classifier input and remain interruptible through the portable
/// cancellation token in both trait families.
#[test]
fn provider_error_body_read_is_bounded_and_cancellable_send_and_local() {
    let mut oversized = vec![b'x'; 128 * 1024];
    oversized.extend_from_slice(b"tail-marker-must-not-be-read");

    let send_observed = Arc::new(Mutex::new(Vec::new()));
    let mut send_request = resolved_request(Vec::new(), Vec::new(), Vec::new(), 1);
    send_request.retry_classifier = Arc::new(InspectingRetryClassifier(Arc::clone(&send_observed)));
    let send_error = run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
            status: 503,
            headers: HeaderMap::new(),
            body: oversized.clone(),
        }])),
        Arc::new(ImmediateSleeper::default()),
        send_request,
    )
    .expect_err("bounded Send rejection");
    assert!(!send_error.message.contains("tail-marker"));
    assert_eq!(lock(&send_observed)[0].len(), 64 * 1024);
    assert!(!lock(&send_observed)[0].contains("tail-marker"));

    let local_observed = Rc::new(RefCell::new(Vec::new()));
    let local_api = LocalHttpChatApi::new(
        Rc::new(RcLocalHandler(Rc::new(Cell::new(0)))),
        Rc::new(StaticLocalHttpTransport {
            status: 503,
            body: oversized,
        }),
    );
    let local_error = block_on(local_api.stream(
        local_resolved_request(
            Rc::new(LocalInspectingRetryClassifier(Rc::clone(&local_observed))),
            1,
        ),
        CancellationToken::new(),
    ))
    .expect_err("bounded local rejection");
    assert!(!local_error.message.contains("tail-marker"));
    assert_eq!(local_observed.borrow()[0].len(), 64 * 1024);
    assert!(!local_observed.borrow()[0].contains("tail-marker"));

    let send_api = HttpChatApi::new(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(PendingErrorBodyTransport),
    );
    let send_cancellation = CancellationToken::new();
    let mut send = Box::pin(send_api.stream(
        resolved_request(Vec::new(), Vec::new(), Vec::new(), 0),
        send_cancellation.clone(),
    ));
    assert!(poll_once(send.as_mut()).is_pending());
    send_cancellation.cancel();
    let Poll::Ready(Err(send_cancelled)) = poll_once(send.as_mut()) else {
        panic!("Send error-body read did not observe cancellation")
    };
    assert_eq!(send_cancelled.kind, AiErrorKind::Cancelled);

    let local_api = LocalHttpChatApi::new(
        Rc::new(RcLocalHandler(Rc::new(Cell::new(0)))),
        Rc::new(LocalPendingErrorBodyTransport),
    );
    let local_cancellation = CancellationToken::new();
    let mut local = Box::pin(local_api.stream(
        local_resolved_request(Rc::new(LocalDefaultRetryClassifier::default()), 0),
        local_cancellation.clone(),
    ));
    assert!(poll_once(local.as_mut()).is_pending());
    local_cancellation.cancel();
    let Poll::Ready(Err(local_cancelled)) = poll_once(local.as_mut()) else {
        panic!("local error-body read did not observe cancellation")
    };
    assert_eq!(local_cancelled.kind, AiErrorKind::Cancelled);
}

#[test]
fn retry_x_should_retry_true_overrides_status() {
    let _basis = RETRY_BASIS;
    assert!(matches!(
        classifier().classify(
            &http_failure(400, &[("x-should-retry", "true")]),
            &RetryPolicy::default()
        ),
        RetryDecision::RetryAfter(_)
    ));
}

#[test]
fn retry_x_should_retry_false_overrides_status() {
    let _basis = RETRY_BASIS;
    assert_eq!(
        classifier().classify(
            &http_failure(429, &[("x-should-retry", "false")]),
            &RetryPolicy::default()
        ),
        RetryDecision::DoNotRetry
    );
}

#[test]
fn retry_transport_failure_without_status() {
    let _basis = RETRY_BASIS;
    let transport = Arc::new(FakeHttpTransport::new([
        TransportOutcome::Failure(TransportError::new("connect", "offline")),
        TransportOutcome::Response {
            status: 200,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
    ]));
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), Vec::new(), Vec::new(), 1),
    )
    .unwrap();
    assert_eq!(transport.request_count(), 2);
}

struct PendingHttpTransport {
    requests: Mutex<Vec<HttpRequest>>,
}

impl PendingHttpTransport {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl HttpTransport for PendingHttpTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        lock(&self.requests).push(request);
        Box::pin(pending())
    }
}

#[test]
fn timeout_ms_is_enforced_per_transport_attempt() {
    let _basis = "packages/ai/src/types.ts:157-164; architecture v2 part 2 §2.5-§2.6";
    let transport = Arc::new(PendingHttpTransport::new());
    let api = HttpChatApi::new(
        Arc::new(TestHandler::new(b"{}")),
        Arc::clone(&transport) as Arc<dyn HttpTransport>,
    )
    .with_retry_sleeper(Arc::new(ImmediateSleeper::default()));
    let mut request = resolved_request(Vec::new(), Vec::new(), Vec::new(), 0);
    request.timeout = Some(Duration::from_millis(1));
    let error = block_on(api.stream(request, CancellationToken::new())).unwrap_err();

    assert_eq!(error.kind, AiErrorKind::Transport);
    assert_eq!(error.provider_code.as_deref(), Some("timeout"));
    assert!(error.retryable);
    assert_eq!(error.retry_after, Some(Duration::from_millis(500)));
    assert_eq!(
        lock(&transport.requests)[0].timeout,
        Some(Duration::from_millis(1))
    );
}

#[test]
fn retry_classifier_decision_is_preserved_in_public_error() {
    let _basis = RETRY_BASIS;
    let retryable = run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
            status: 400,
            headers: headers(&[("x-should-retry", "true"), ("x-request-id", "req-400")]),
            body: Vec::new(),
        }])),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), Vec::new(), Vec::new(), 0),
    )
    .unwrap_err();
    assert_eq!(retryable.kind, AiErrorKind::ProviderRejected);
    assert!(retryable.retryable);
    assert_eq!(retryable.retry_after, Some(Duration::from_millis(500)));
    assert_eq!(retryable.status, Some(400));
    assert_eq!(retryable.request_id.as_deref(), Some("req-400"));
    assert_eq!(retryable.attempt, Some(0));

    let not_retryable = run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
            status: 429,
            headers: headers(&[("x-should-retry", "false")]),
            body: Vec::new(),
        }])),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), Vec::new(), Vec::new(), 0),
    )
    .unwrap_err();
    assert_eq!(not_retryable.kind, AiErrorKind::RateLimited);
    assert!(!not_retryable.retryable);
    assert_eq!(not_retryable.retry_after, None);
    assert_eq!(not_retryable.status, Some(429));
}

#[test]
fn retry_http_408() {
    let _basis = RETRY_BASIS;
    assert_retry(408);
}

#[test]
fn retry_http_409() {
    let _basis = RETRY_BASIS;
    assert_retry(409);
}

#[test]
fn retry_http_429() {
    let _basis = RETRY_BASIS;
    assert_retry(429);
}

#[test]
fn retry_http_500_through_599() {
    let _basis = RETRY_BASIS;
    assert_retry(500);
    assert_retry(599);
    // Pinned Pi uses `status >= 500`, including nonstandard numeric statuses.
    assert_retry(600);
}

#[test]
fn retry_non_retryable_4xx() {
    let _basis = RETRY_BASIS;
    for status in [400, 401, 403, 404, 422] {
        assert_no_retry(status);
    }
}

#[test]
fn retry_after_ms_precedes_retry_after() {
    let _basis = RETRY_BASIS;
    assert_eq!(
        classifier().classify(
            &http_failure(429, &[("retry-after-ms", "12.5"), ("retry-after", "9")]),
            &RetryPolicy::default()
        ),
        RetryDecision::RetryAfter(Duration::from_micros(12_500))
    );
}

#[test]
fn retry_after_accepts_decimal_seconds() {
    let _basis = RETRY_BASIS;
    assert_eq!(
        classifier().classify(
            &http_failure(429, &[("retry-after", "1.5")]),
            &RetryPolicy::default()
        ),
        RetryDecision::RetryAfter(Duration::from_millis(1_500))
    );
}

#[test]
fn retry_after_accepts_http_date() {
    let _basis = RETRY_BASIS;
    let observed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let requested = observed + Duration::from_secs(17);
    let failure = AttemptFailure::http_at(
        0,
        429,
        headers(&[("retry-after", &httpdate::fmt_http_date(requested))]),
        observed,
        "fixture",
    );
    assert_eq!(
        classifier().classify(&failure, &RetryPolicy::default()),
        RetryDecision::RetryAfter(Duration::from_secs(17))
    );
}

#[test]
fn retry_server_delay_over_max_fails_immediately() {
    let _basis = RETRY_BASIS;
    let policy = RetryPolicy {
        max_server_delay: Some(Duration::from_secs(1)),
        ..RetryPolicy::default()
    };
    assert_eq!(
        classifier().classify(&http_failure(429, &[("retry-after", "2")]), &policy),
        RetryDecision::RejectServerDelay {
            requested: Duration::from_secs(2),
            maximum: Duration::from_secs(1),
        }
    );
}

#[test]
fn retry_zero_max_delay_disables_cap() {
    let _basis = RETRY_BASIS;
    let policy = RetryPolicy {
        max_server_delay: Some(Duration::ZERO),
        ..RetryPolicy::default()
    };
    assert_eq!(
        classifier().classify(&http_failure(429, &[("retry-after", "277403")]), &policy),
        RetryDecision::RetryAfter(Duration::from_secs(277_403))
    );
}

#[test]
fn retry_exponential_sequence_matches_pi() {
    let _basis = RETRY_BASIS;
    let classifier = classifier();
    let policy = RetryPolicy::default();
    let actual = (0..6)
        .map(|attempt| {
            let failure =
                AttemptFailure::transport(attempt, TransportError::new("connect", "offline"));
            match classifier.classify(&failure, &policy) {
                RetryDecision::RetryAfter(delay) => delay,
                decision => panic!("unexpected retry decision: {decision:?}"),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [500, 1_000, 2_000, 4_000, 8_000, 8_000].map(Duration::from_millis)
    );
}

#[test]
fn retry_jitter_range_matches_pi() {
    let _basis = RETRY_BASIS;
    let failure = AttemptFailure::transport(0, TransportError::new("connect", "offline"));
    let policy = RetryPolicy::default();
    assert_eq!(
        DefaultRetryClassifier::new(FixedJitter(0.75)).classify(&failure, &policy),
        RetryDecision::RetryAfter(Duration::from_millis(375))
    );
    assert_eq!(
        DefaultRetryClassifier::new(FixedJitter(1.0)).classify(&failure, &policy),
        RetryDecision::RetryAfter(Duration::from_millis(500))
    );
}

#[test]
fn retry_cancellation_before_attempt() {
    let _basis = RETRY_BASIS;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let calls = AtomicUsize::new(0);
    let result = block_on(establish_with_retry_and_sleeper(
        &RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        },
        &classifier(),
        &ImmediateSleeper::default(),
        &cancellation,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, AttemptFailure>(()) }
        },
    ));
    assert!(matches!(result, Err(AttemptFailure::Cancelled)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn retry_cancellation_during_request() {
    let _basis = RETRY_BASIS;
    let cancellation = CancellationToken::new();
    let policy = RetryPolicy::default();
    let classifier = classifier();
    let sleeper = ImmediateSleeper::default();
    let mut operation = Box::pin(establish_with_retry_and_sleeper(
        &policy,
        &classifier,
        &sleeper,
        &cancellation,
        |_| async { pending::<Result<(), AttemptFailure>>().await },
    ));
    assert!(poll_once(operation.as_mut()).is_pending());
    cancellation.cancel();
    assert!(matches!(
        poll_once(operation.as_mut()),
        Poll::Ready(Err(AttemptFailure::Cancelled))
    ));
}

#[test]
fn retry_cancellation_during_backoff() {
    let _basis = RETRY_BASIS;
    let cancellation = CancellationToken::new();
    let policy = RetryPolicy {
        max_retries: 1,
        ..RetryPolicy::default()
    };
    let classifier = classifier();
    let mut operation = Box::pin(establish_with_retry_and_sleeper(
        &policy,
        &classifier,
        &PendingSleeper,
        &cancellation,
        |attempt| async move {
            Err::<(), _>(AttemptFailure::transport(
                attempt,
                TransportError::new("connect", "offline"),
            ))
        },
    ));
    assert!(poll_once(operation.as_mut()).is_pending());
    cancellation.cancel();
    assert!(matches!(
        poll_once(operation.as_mut()),
        Poll::Ready(Err(AttemptFailure::Cancelled))
    ));
}

fn poll_once<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
    let waker = noop_waker();
    let mut context = TaskContext::from_waker(&waker);
    future.poll(&mut context)
}

#[test]
fn retry_never_restarts_after_semantic_event() {
    let _basis = RETRY_BASIS;
    let mut handler = TestHandler::new(b"{}");
    handler.events = failed_events();
    let handler = Arc::new(handler);
    let transport = Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
        status: 200,
        headers: HeaderMap::new(),
        body: Vec::new(),
    }]));
    let stream = run_http(
        handler,
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), Vec::new(), Vec::new(), 3),
    )
    .unwrap();
    let events = block_on(stream.collect::<Vec<_>>());
    assert!(matches!(events.last(), Some(AssistantEvent::Failed { .. })));
    assert_eq!(transport.request_count(), 1);
}

fn failed_events() -> Vec<AssistantEvent> {
    let provider = ProviderId::new("test-provider");
    let api = ApiId::new("test-api");
    let model = ModelId::new("test-model");
    let message = AssistantMessage {
        id: MessageId::new("message-1"),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: model.clone(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: Vec::new(),
        replay: ReplayEnvelope::new(ReplayScope::new(
            provider.clone(),
            api.clone(),
            model.clone(),
            model.clone(),
        )),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Error,
            raw_provider_reason: None,
            error: Some(PublicError {
                code: "stream_reset".into(),
                message: "stream reset".into(),
                retryable: true,
                provider_code: None,
                status: None,
                request_id: None,
            }),
        },
        timestamp: Timestamp::from_unix_millis(0),
    };
    vec![
        AssistantEvent::MessageStarted {
            message_id: message.id.clone(),
            provider,
            api,
            model,
        },
        AssistantEvent::Failed { message },
    ]
}

struct AttemptHeader;

impl AttemptMiddleware for AttemptHeader {
    fn before_attempt<'a>(
        &'a self,
        attempt: u32,
        request: &'a mut HttpRequest,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            request.headers.insert(
                HeaderName::from_static("x-attempt"),
                HeaderValue::from_str(&attempt.to_string()).unwrap(),
            );
            Ok(())
        })
    }
}

struct HostileCredentialOverlay;

impl AttemptMiddleware for HostileCredentialOverlay {
    fn before_attempt<'a>(
        &'a self,
        _attempt: u32,
        request: &'a mut HttpRequest,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        request.headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer hostile-logical-overlay"),
        );
        request.auth_headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer hostile-snapshot-overlay"),
        );
        Box::pin(async { Ok(()) })
    }
}

/// Architecture v2 part 2 §2.6 correction; Codex's final transport must see
/// the credential contribution even if attempt middleware mutates every
/// public request field before dispatch.
#[test]
fn attempt_middleware_cannot_replace_credential_header_snapshot() {
    let transport = Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
        status: 200,
        headers: HeaderMap::new(),
        body: Vec::new(),
    }]));
    let mut request = resolved_request(
        Vec::new(),
        Vec::new(),
        vec![Arc::new(HostileCredentialOverlay)],
        0,
    );
    request.auth_headers = headers(&[("authorization", "Bearer credential")]);
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        request,
    )
    .unwrap();

    let requests = lock(&transport.requests);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer hostile-logical-overlay"
    );
    assert_eq!(
        requests[0].auth_headers["authorization"],
        "Bearer credential"
    );
}

#[test]
fn retry_fresh_transport_attempt_number() {
    let _basis = RETRY_BASIS;
    let transport = Arc::new(FakeHttpTransport::new([
        TransportOutcome::Response {
            status: 500,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
        TransportOutcome::Response {
            status: 200,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
    ]));
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), Vec::new(), vec![Arc::new(AttemptHeader)], 1),
    )
    .unwrap();
    let requests = lock(&transport.requests);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.attempt)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(requests[0].headers["x-attempt"], "0");
    assert_eq!(requests[1].headers["x-attempt"], "1");
}

struct StaticAuth {
    auth: ResolvedAuth,
}

impl AuthResolver for StaticAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let auth = self.auth.clone();
        Box::pin(async move { Ok(Some(auth)) })
    }
}

#[derive(Clone)]
struct CapturedRequest {
    headers: HeaderMap,
    endpoint: Url,
    max_retries: u32,
    timeout: Option<Duration>,
}

#[derive(Default)]
struct RecordingChatApi {
    captured: Mutex<Vec<CapturedRequest>>,
}

impl ChatApi for RecordingChatApi {
    fn stream(
        &self,
        request: ResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        lock(&self.captured).push(CapturedRequest {
            headers: request.headers,
            endpoint: request.endpoint,
            max_retries: request.retry_policy.max_retries,
            timeout: request.timeout,
        });
        Box::pin(async { Ok(AssistantStream::new(futures_util::stream::empty())) })
    }
}

struct HeaderEditor {
    count: Arc<AtomicUsize>,
    remove: Option<HeaderName>,
    insert: Option<(HeaderName, HeaderValue)>,
    expected: Option<(HeaderName, HeaderValue)>,
}

impl HeaderTransform for HeaderEditor {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            if let Some((name, value)) = &self.expected {
                assert_eq!(headers.get(name), Some(value));
            }
            if let Some(name) = &self.remove {
                headers.remove(name);
            }
            if let Some((name, value)) = &self.insert {
                headers.insert(name, value.clone());
            }
            Ok(())
        })
    }
}

fn routed_request(
    model_headers: HeaderMapSpec,
    explicit_headers: HeaderMapSpec,
    auth_headers: HeaderMap,
    provider_headers: HeaderMapSpec,
    transforms: Vec<Arc<dyn HeaderTransform>>,
) -> (Arc<RecordingChatApi>, CapturedRequest) {
    let mut descriptor = model();
    descriptor.common.headers = model_headers;
    let chat = Arc::new(RecordingChatApi::default());
    let registration = ProviderRegistration::builder("test-provider")
        .headers(provider_headers)
        .auth(Arc::new(StaticAuth {
            auth: ResolvedAuth {
                api_key: None,
                headers: auth_headers,
                base_url: Some(Url::parse("https://auth.example/v1").unwrap()),
                source: AuthSource::new("fixture"),
            },
        }))
        .models(vec![descriptor])
        .api(ApiId::new("test-api"), chat.clone())
        .build()
        .unwrap();
    let mut builder = Models::builder().provider(registration);
    for transform in transforms {
        builder = builder.header_transform(transform);
    }
    let models = builder.build().unwrap();
    let options = SimpleGenerationOptions {
        headers: explicit_headers,
        max_retries: Some(2),
        timeout_ms: Some(321),
        ..SimpleGenerationOptions::default()
    };
    block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: ModelRef::new("test-provider", "test-model"),
            context: pi_ai::Context::new(None),
            options,
        },
        CancellationToken::new(),
    ))
    .unwrap();
    let captured = lock(&chat.captured)[0].clone();
    (chat, captured)
}

#[test]
fn headers_merge_case_insensitively() {
    let _basis = MIDDLEWARE_BASIS;
    let mut actual = headers(&[("authorization", "old")]);
    apply_header_spec(&mut actual, &header_spec(&[("Authorization", Some("new"))])).unwrap();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual["authorization"], "new");
}

#[test]
fn headers_auth_before_model() {
    let _basis = MIDDLEWARE_BASIS;
    let (_, captured) = routed_request(
        header_spec(&[("x-order", Some("model"))]),
        HeaderMapSpec::new(),
        headers(&[("x-order", "auth")]),
        HeaderMapSpec::new(),
        Vec::new(),
    );
    assert_eq!(captured.headers["x-order"], "model");
}

#[test]
fn headers_model_before_explicit() {
    let _basis = MIDDLEWARE_BASIS;
    let (_, captured) = routed_request(
        header_spec(&[("x-order", Some("model"))]),
        header_spec(&[("X-Order", Some("explicit"))]),
        HeaderMap::new(),
        HeaderMapSpec::new(),
        Vec::new(),
    );
    assert_eq!(captured.headers["x-order"], "explicit");
}

#[test]
fn headers_explicit_before_transform() {
    let _basis = MIDDLEWARE_BASIS;
    let count = Arc::new(AtomicUsize::new(0));
    let (_, captured) = routed_request(
        HeaderMapSpec::new(),
        header_spec(&[("x-order", Some("explicit"))]),
        HeaderMap::new(),
        HeaderMapSpec::new(),
        vec![Arc::new(HeaderEditor {
            count,
            remove: None,
            insert: Some((
                HeaderName::from_static("x-order"),
                HeaderValue::from_static("transform"),
            )),
            expected: Some((
                HeaderName::from_static("x-order"),
                HeaderValue::from_static("explicit"),
            )),
        })],
    );
    assert_eq!(captured.headers["x-order"], "transform");
}

#[test]
fn headers_transform_can_delete_default() {
    let _basis = MIDDLEWARE_BASIS;
    let (_, captured) = routed_request(
        HeaderMapSpec::new(),
        HeaderMapSpec::new(),
        HeaderMap::new(),
        header_spec(&[("x-default", Some("present"))]),
        vec![Arc::new(HeaderEditor {
            count: Arc::new(AtomicUsize::new(0)),
            remove: Some(HeaderName::from_static("x-default")),
            insert: None,
            expected: None,
        })],
    );
    assert!(!captured.headers.contains_key("x-default"));
}

#[test]
fn headers_transform_runs_once() {
    let _basis = MIDDLEWARE_BASIS;
    let count = Arc::new(AtomicUsize::new(0));
    let (_, captured) = routed_request(
        HeaderMapSpec::new(),
        HeaderMapSpec::new(),
        HeaderMap::new(),
        HeaderMapSpec::new(),
        vec![Arc::new(HeaderEditor {
            count: Arc::clone(&count),
            remove: None,
            insert: None,
            expected: None,
        })],
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(captured.max_retries, 2);
}

#[test]
fn headers_transform_not_forwarded_to_provider_options() {
    let _basis = MIDDLEWARE_BASIS;
    let count = Arc::new(AtomicUsize::new(0));
    let (_, captured) = routed_request(
        HeaderMapSpec::new(),
        HeaderMapSpec::new(),
        HeaderMap::new(),
        HeaderMapSpec::new(),
        vec![Arc::new(HeaderEditor {
            count: Arc::clone(&count),
            remove: None,
            insert: Some((
                HeaderName::from_static("x-consumed"),
                HeaderValue::from_static("yes"),
            )),
            expected: None,
        })],
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(captured.headers["x-consumed"], "yes");
    assert_eq!(captured.endpoint.as_str(), "https://auth.example/v1");
}

#[test]
fn timeout_ms_is_forwarded_by_models() {
    let _basis = "packages/ai/src/models.ts:621-696; packages/ai/src/types.ts:157-164; architecture v2 part 2 §2.5-§2.6";
    let (_, captured) = routed_request(
        HeaderMapSpec::new(),
        HeaderMapSpec::new(),
        HeaderMap::new(),
        HeaderMapSpec::new(),
        Vec::new(),
    );
    assert_eq!(captured.timeout, Some(Duration::from_millis(321)));
}

enum PayloadEdit {
    Append(&'static str),
    Replace(&'static str),
}

struct PayloadMiddleware {
    edit: PayloadEdit,
    count: Arc<AtomicUsize>,
    order: Option<(Arc<Mutex<Vec<&'static str>>>, &'static str)>,
}

impl PayloadTransform<TestApi> for PayloadMiddleware {
    fn transform<'a>(
        &'a self,
        _context: PayloadTransformContext<'a, TestApi>,
        payload: &'a mut TestWireRequest,
    ) -> SendBoxFuture<'a, Result<PayloadTransformResult<TestWireRequest>, MiddlewareError>> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            if let Some((order, name)) = &self.order {
                lock(order).push(name);
            }
            match self.edit {
                PayloadEdit::Append(value) => {
                    payload.body.push_str(value);
                    Ok(PayloadTransformResult::Continue)
                }
                PayloadEdit::Replace(value) => {
                    Ok(PayloadTransformResult::Replace(TestWireRequest {
                        body: value.into(),
                    }))
                }
            }
        })
    }
}

fn typed_payload_transform(transform: PayloadMiddleware) -> Arc<dyn ErasedPayloadTransform> {
    Arc::new(PayloadTransformAdapter::<TestApi>::new(Arc::new(transform)))
}

fn success_transport() -> Arc<FakeHttpTransport> {
    Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
        status: 200,
        headers: HeaderMap::new(),
        body: Vec::new(),
    }]))
}

#[test]
fn payload_in_place_mutation_is_retained() {
    let _basis = MIDDLEWARE_BASIS;
    let transport = success_transport();
    run_http(
        Arc::new(TestHandler::new(b"a")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            vec![typed_payload_transform(PayloadMiddleware {
                edit: PayloadEdit::Append("b"),
                count: Arc::new(AtomicUsize::new(0)),
                order: None,
            })],
            Vec::new(),
            Vec::new(),
            0,
        ),
    )
    .unwrap();
    assert_eq!(lock(&transport.requests)[0].body, b"ab");
}

#[test]
fn typed_payload_transform_is_adapted_by_models() {
    let _basis = MIDDLEWARE_BASIS;
    let transport = success_transport();
    let handler = Arc::new(TestHandler::new(b"a"));
    let chat = Arc::new(
        HttpChatApi::new(
            Arc::clone(&handler) as Arc<dyn ErasedApiHandler>,
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
        )
        .with_retry_sleeper(Arc::new(ImmediateSleeper::default())),
    );
    let registration = ProviderRegistration::builder("test-provider")
        .models(vec![model()])
        .api(ApiId::new("test-api"), chat)
        .build()
        .unwrap();
    let models = Models::builder()
        .provider(registration)
        .payload_transform::<TestApi>(Arc::new(PayloadMiddleware {
            edit: PayloadEdit::Append("b"),
            count: Arc::new(AtomicUsize::new(0)),
            order: None,
        }))
        .build()
        .unwrap();

    block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: ModelRef::new("test-provider", "test-model"),
            context: pi_ai::Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .unwrap();

    assert_eq!(handler.encode_count.load(Ordering::SeqCst), 1);
    assert_eq!(lock(&transport.requests)[0].body, b"ab");
}

#[test]
fn payload_replacement_is_retained() {
    let _basis = MIDDLEWARE_BASIS;
    let transport = success_transport();
    run_http(
        Arc::new(TestHandler::new(b"original")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            vec![typed_payload_transform(PayloadMiddleware {
                edit: PayloadEdit::Replace("replacement"),
                count: Arc::new(AtomicUsize::new(0)),
                order: None,
            })],
            Vec::new(),
            Vec::new(),
            0,
        ),
    )
    .unwrap();
    assert_eq!(lock(&transport.requests)[0].body, b"replacement");
}

#[test]
fn payload_transforms_run_in_registration_order() {
    let _basis = MIDDLEWARE_BASIS;
    let order = Arc::new(Mutex::new(Vec::new()));
    let transport = success_transport();
    run_http(
        Arc::new(TestHandler::new(Vec::new())),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            vec![
                typed_payload_transform(PayloadMiddleware {
                    edit: PayloadEdit::Append("a"),
                    count: Arc::new(AtomicUsize::new(0)),
                    order: Some((Arc::clone(&order), "a")),
                }),
                typed_payload_transform(PayloadMiddleware {
                    edit: PayloadEdit::Append("b"),
                    count: Arc::new(AtomicUsize::new(0)),
                    order: Some((Arc::clone(&order), "b")),
                }),
            ],
            Vec::new(),
            Vec::new(),
            0,
        ),
    )
    .unwrap();
    assert_eq!(*lock(&order), ["a", "b"]);
    assert_eq!(lock(&transport.requests)[0].body, b"ab");
}

#[test]
fn payload_transform_runs_once_per_logical_request() {
    let _basis = MIDDLEWARE_BASIS;
    let count = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(FakeHttpTransport::new([
        TransportOutcome::Response {
            status: 500,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
        TransportOutcome::Response {
            status: 200,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
    ]));
    run_http(
        Arc::new(TestHandler::new(b"a")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            vec![typed_payload_transform(PayloadMiddleware {
                edit: PayloadEdit::Append("b"),
                count: Arc::clone(&count),
                order: None,
            })],
            Vec::new(),
            Vec::new(),
            1,
        ),
    )
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(
        lock(&transport.requests)
            .iter()
            .all(|request| request.body == b"ab")
    );
}

struct CountingAttemptMiddleware(Arc<AtomicUsize>);

impl AttemptMiddleware for CountingAttemptMiddleware {
    fn before_attempt<'a>(
        &'a self,
        _attempt: u32,
        _request: &'a mut HttpRequest,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn attempt_middleware_runs_per_retry() {
    let _basis = MIDDLEWARE_BASIS;
    let count = Arc::new(AtomicUsize::new(0));
    let transport = Arc::new(FakeHttpTransport::new([
        TransportOutcome::Response {
            status: 500,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
        TransportOutcome::Response {
            status: 200,
            headers: HeaderMap::new(),
            body: Vec::new(),
        },
    ]));
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        transport,
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            Vec::new(),
            Vec::new(),
            vec![Arc::new(CountingAttemptMiddleware(Arc::clone(&count)))],
            1,
        ),
    )
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

struct RecordingObserver {
    attempts: Arc<Mutex<Vec<u32>>>,
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl ResponseObserver for RecordingObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        self.attempts.lock().unwrap().push(response.attempt);
        lock(&self.headers).push(response.headers.clone());
        if let Some(log) = &self.log {
            lock(log).push(format!("observe:{}", response.attempt));
        }
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn response_observer_runs_before_body_consumption() {
    let _basis = MIDDLEWARE_BASIS;
    let log = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(RecordingObserver {
        attempts: Arc::new(Mutex::new(Vec::new())),
        headers: Arc::new(Mutex::new(Vec::new())),
        log: Some(Arc::clone(&log)),
    });
    let mut handler = TestHandler::new(b"{}");
    handler.log = Some(Arc::clone(&log));
    run_http(
        Arc::new(handler),
        Arc::new(FakeHttpTransport::with_log(
            [TransportOutcome::Response {
                status: 200,
                headers: HeaderMap::new(),
                body: b"body".to_vec(),
            }],
            Arc::clone(&log),
        )),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), vec![observer], Vec::new(), 0),
    )
    .unwrap();
    assert_eq!(*lock(&log), ["transport:0", "observe:0", "decode"]);
}

#[test]
fn response_observer_runs_for_retry_responses() {
    let _basis = MIDDLEWARE_BASIS;
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(RecordingObserver {
        attempts: Arc::clone(&attempts),
        headers: Arc::new(Mutex::new(Vec::new())),
        log: None,
    });
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([
            TransportOutcome::Response {
                status: 429,
                headers: HeaderMap::new(),
                body: Vec::new(),
            },
            TransportOutcome::Response {
                status: 200,
                headers: HeaderMap::new(),
                body: Vec::new(),
            },
        ])),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), vec![observer], Vec::new(), 1),
    )
    .unwrap();
    assert_eq!(*lock(&attempts), [0, 1]);
}

#[test]
fn injected_http_transport_receives_final_request() {
    let _basis = MIDDLEWARE_BASIS;
    let transport = success_transport();
    run_http(
        Arc::new(TestHandler::new(b"logical")),
        Arc::clone(&transport),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(
            vec![typed_payload_transform(PayloadMiddleware {
                edit: PayloadEdit::Append("-payload"),
                count: Arc::new(AtomicUsize::new(0)),
                order: None,
            })],
            Vec::new(),
            vec![Arc::new(AttemptHeader)],
            0,
        ),
    )
    .unwrap();
    let requests = lock(&transport.requests);
    assert_eq!(requests[0].url.as_str(), "https://effective.example/v1");
    assert_eq!(requests[0].body, b"logical-payload");
    assert_eq!(requests[0].headers["x-attempt"], "0");
}

#[test]
fn bedrock_custom_headers_are_inserted_before_signing() {
    let _basis = BEDROCK_BASIS;
    let logical = headers(&[("x-custom", "value")]);
    let mut serialized = headers(&[("host", "bedrock.example")]);
    apply_bedrock_signer_headers(&logical, &mut serialized);
    assert_eq!(serialized["x-custom"], "value");
    let signed_names = serialized
        .keys()
        .map(HeaderName::as_str)
        .collect::<Vec<_>>();
    assert!(signed_names.contains(&"x-custom"));
}

#[test]
fn bedrock_reserved_headers_are_suppressed() {
    let _basis = BEDROCK_BASIS;
    let logical = headers(&[
        ("authorization", "evil"),
        ("host", "evil"),
        ("x-amz-date", "evil"),
        ("x-allowed", "ok"),
    ]);
    let mut serialized = headers(&[
        ("authorization", "real-auth"),
        ("host", "real-host"),
        ("x-amz-date", "real-date"),
    ]);
    apply_bedrock_signer_headers(&logical, &mut serialized);
    assert_eq!(serialized["authorization"], "real-auth");
    assert_eq!(serialized["host"], "real-host");
    assert_eq!(serialized["x-amz-date"], "real-date");
    assert_eq!(serialized["x-allowed"], "ok");
}

#[test]
fn bedrock_response_observer_receives_raw_headers() {
    let _basis = BEDROCK_BASIS;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::new(RecordingObserver {
        attempts: Arc::new(Mutex::new(Vec::new())),
        headers: Arc::clone(&observed),
        log: None,
    });
    run_http(
        Arc::new(TestHandler::new(b"{}")),
        Arc::new(FakeHttpTransport::new([TransportOutcome::Response {
            status: 200,
            headers: headers(&[
                ("x-amzn-requestid", "req-123"),
                ("x-bifrost-provider", "bedrock"),
                ("x-bifrost-resolved-model", "model-1"),
            ]),
            body: Vec::new(),
        }])),
        Arc::new(ImmediateSleeper::default()),
        resolved_request(Vec::new(), vec![observer], Vec::new(), 0),
    )
    .unwrap();
    let observed = lock(&observed);
    assert_eq!(observed[0]["x-amzn-requestid"], "req-123");
    assert_eq!(observed[0]["x-bifrost-provider"], "bedrock");
    assert_eq!(observed[0]["x-bifrost-resolved-model"], "model-1");
}

struct RegistryProbeAuth {
    models: Arc<Mutex<Option<Models>>>,
}

impl AuthResolver for RegistryProbeAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let mut yielded = false;
            futures_util::future::poll_fn(|context| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            assert_eq!(
                lock(&self.models)
                    .as_ref()
                    .expect("models installed")
                    .providers()
                    .len(),
                1
            );
            Ok(Some(ResolvedAuth {
                api_key: None,
                headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("probe"),
            }))
        })
    }
}

#[test]
fn models_registry_lock_is_released_before_auth_await() {
    let _basis = "packages/ai/src/models.ts:621-696; architecture v2 part 1 §3.6";
    let slot = Arc::new(Mutex::new(None));
    let chat = Arc::new(RecordingChatApi::default());
    let registration = ProviderRegistration::builder("test-provider")
        .auth(Arc::new(RegistryProbeAuth {
            models: Arc::clone(&slot),
        }))
        .models(vec![model()])
        .api(ApiId::new("test-api"), chat)
        .build()
        .unwrap();
    let models = Models::builder().provider(registration).build().unwrap();
    *lock(&slot) = Some(models.clone());

    block_on(ModelRuntime::stream(
        &models,
        ModelRequest {
            model: ModelRef::new("test-provider", "test-model"),
            context: pi_ai::Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .unwrap();
}

#[test]
fn models_provider_registration_is_atomic() {
    let _basis = "packages/ai/src/models.ts:245-278; architecture v2 part 1 §3.6";
    let models = Models::default();
    let invalid = ProviderRegistration {
        descriptor: ProviderDescriptor::new("test-provider"),
        auth: Arc::new(AnonymousAuthResolver),
        catalog: Arc::new(StaticModelCatalog::new(vec![model()])),
        apis: std::collections::HashMap::new(),
        retry_policy: RetryPolicy::default(),
        retry_classifier: Arc::new(classifier()),
    };
    assert!(matches!(
        models.set_provider(invalid),
        Err(ProviderRegistrationError::MissingApi { .. })
    ));
    assert!(models.providers().is_empty());
}

struct RcLocalAuth(Rc<Cell<usize>>);

impl LocalAuthResolver for RcLocalAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        self.0.set(self.0.get() + 1);
        Box::pin(async {
            Ok(Some(ResolvedAuth {
                api_key: None,
                headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("local"),
            }))
        })
    }
}

struct RcLocalHeader(Rc<Cell<usize>>);

impl LocalHeaderTransform for RcLocalHeader {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        self.0.set(self.0.get() + 1);
        headers.insert("x-local", HeaderValue::from_static("yes"));
        Box::pin(async { Ok(()) })
    }
}

struct RcLocalPayload(Rc<Cell<usize>>);

impl LocalPayloadTransform<TestApi> for RcLocalPayload {
    fn transform<'a>(
        &'a self,
        _context: PayloadTransformContext<'a, TestApi>,
        payload: &'a mut TestWireRequest,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformResult<TestWireRequest>, MiddlewareError>> {
        self.0.set(self.0.get() + 1);
        payload.body.push('b');
        Box::pin(async { Ok(PayloadTransformResult::Continue) })
    }
}

struct RcLocalAttempt(Rc<Cell<usize>>);

impl LocalAttemptMiddleware for RcLocalAttempt {
    fn before_attempt<'a>(
        &'a self,
        _attempt: u32,
        _request: &'a mut HttpRequest,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        self.0.set(self.0.get() + 1);
        Box::pin(async { Ok(()) })
    }
}

struct RcLocalObserver(Rc<Cell<usize>>);

impl LocalResponseObserver for RcLocalObserver {
    fn on_response<'a>(
        &'a self,
        _context: ResponseObservationContext<'a>,
        _response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        self.0.set(self.0.get() + 1);
        Box::pin(async { Ok(()) })
    }
}

struct RcLocalClassifier(Rc<Cell<usize>>);

impl LocalRetryClassifier for RcLocalClassifier {
    fn classify(&self, _failure: &AttemptFailure, _policy: &RetryPolicy) -> RetryDecision {
        self.0.set(self.0.get() + 1);
        RetryDecision::RetryAfter(Duration::ZERO)
    }
}

struct RcLocalSleeper(Rc<Cell<usize>>);

impl LocalRetrySleeper for RcLocalSleeper {
    fn sleep(
        &self,
        _duration: Duration,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AttemptFailure>> {
        self.0.set(self.0.get() + 1);
        Box::pin(async { Ok(()) })
    }
}

struct RcLocalTransport {
    statuses: RefCell<VecDeque<u16>>,
    requests: Rc<RefCell<Vec<HttpRequest>>>,
}

impl LocalHttpTransport for RcLocalTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.requests.borrow_mut().push(request);
        let status = self.statuses.borrow_mut().pop_front().unwrap();
        Box::pin(async move { Ok(LocalHttpResponse::empty(status, HeaderMap::new())) })
    }
}

struct RcLocalHandler(Rc<Cell<usize>>);

impl LocalErasedApiHandler for RcLocalHandler {
    fn api_id(&self) -> &ApiId {
        static API: std::sync::LazyLock<ApiId> =
            std::sync::LazyLock::new(|| ApiId::new("test-api"));
        &API
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        _context: &pi_ai::Context,
        _simple: &SimpleGenerationOptions,
        _patch: Option<&ErasedApiOptionsPatch>,
        _execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        self.0.set(self.0.get() + 1);
        Ok(ProviderPayload::typed(
            Method::POST,
            typed_test_model(model),
            TestWireRequest { body: "a".into() },
            |wire_request| Ok(wire_request.body.as_bytes().to_vec()),
        ))
    }

    fn decode_stream(
        &self,
        _response: LocalProviderResponseStream,
        _execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        LocalAssistantStream::new(futures_util::stream::empty())
    }
}

#[test]
fn local_provider_components_accept_rc_state() {
    let _basis = "architecture v2 part 2 §9.2; §2.4-§2.6 local trait family";
    assert_not_impl_any!(LocalModels: Send, Sync);

    let auth_count = Rc::new(Cell::new(0));
    let header_count = Rc::new(Cell::new(0));
    let payload_count = Rc::new(Cell::new(0));
    let attempt_count = Rc::new(Cell::new(0));
    let observer_count = Rc::new(Cell::new(0));
    let classifier_count = Rc::new(Cell::new(0));
    let sleeper_count = Rc::new(Cell::new(0));
    let handler_count = Rc::new(Cell::new(0));
    let requests = Rc::new(RefCell::new(Vec::new()));

    let http = Rc::new(
        LocalHttpChatApi::new(
            Rc::new(RcLocalHandler(Rc::clone(&handler_count))),
            Rc::new(RcLocalTransport {
                statuses: RefCell::new(VecDeque::from([500, 200])),
                requests: Rc::clone(&requests),
            }),
        )
        .with_retry_sleeper(Rc::new(RcLocalSleeper(Rc::clone(&sleeper_count)))),
    );
    let registration = LocalProviderRegistration::builder("test-provider")
        .auth(Rc::new(RcLocalAuth(Rc::clone(&auth_count))))
        .models(vec![model()])
        .api(ApiId::new("test-api"), http)
        .retry_policy(RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        })
        .retry_classifier(Rc::new(RcLocalClassifier(Rc::clone(&classifier_count))))
        .build()
        .unwrap();
    let models = LocalModels::builder()
        .provider(registration)
        .header_transform(Rc::new(RcLocalHeader(Rc::clone(&header_count))))
        .payload_transform::<TestApi>(Rc::new(RcLocalPayload(Rc::clone(&payload_count))))
        .attempt_middleware(Rc::new(RcLocalAttempt(Rc::clone(&attempt_count))))
        .response_observer(Rc::new(RcLocalObserver(Rc::clone(&observer_count))))
        .build()
        .unwrap();

    block_on(LocalModelRuntime::stream(
        &models,
        ModelRequest {
            model: ModelRef::new("test-provider", "test-model"),
            context: pi_ai::Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ))
    .unwrap();

    assert_eq!(auth_count.get(), 1);
    assert_eq!(header_count.get(), 1);
    assert_eq!(payload_count.get(), 1);
    assert_eq!(attempt_count.get(), 2);
    assert_eq!(observer_count.get(), 2);
    assert_eq!(classifier_count.get(), 1);
    assert_eq!(sleeper_count.get(), 1);
    assert_eq!(handler_count.get(), 1);
    assert_eq!(requests.borrow().len(), 2);
    assert!(
        requests
            .borrow()
            .iter()
            .all(|request| request.body == b"ab")
    );
    assert!(
        requests
            .borrow()
            .iter()
            .all(|request| request.headers["x-local"] == "yes")
    );
}
