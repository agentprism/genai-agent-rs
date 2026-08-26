use agentprism_ai::{
    ApiFamily, ApiId, ApiRequestOptions, AttemptFailure, CancellationToken, ChatApi, Context,
    HttpChatApi, HttpRequest, HttpResponse, HttpTransport, LocalBoxFuture, LocalChatApi,
    LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport, LocalResolvedApiRequest,
    LocalRetryClassifier, LocalRetrySleeper, OpenAiCodexResponses, ResolvedApiRequest,
    RetryClassifier, RetryDecision, RetrySleeper, SendBoxFuture, SimpleGenerationOptions,
    StreamTransport, TransportError,
};
use agentprism_openai::{
    LocalOpenAiCodexResponsesTransport, LocalOpenAiCodexRetryClassifier,
    OpenAiCodexResponsesHandler, OpenAiCodexResponsesTransport, OpenAiCodexRetryClassifier,
    openai_codex_responses_api, openai_codex_retry_policy,
};
use agentprism_openai_codex::openai_codex_models;
use http::{HeaderMap, HeaderValue};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Architecture v2 part 2 §2.4; pinned Pi basis:
/// `openai-codex-responses.ts:isRetryableError`,
/// `isTerminalRateLimitError`, and `getRetryDelay`.
#[test]
fn openai_codex_retry_classification_pi_exact_send() {
    let classifier = OpenAiCodexRetryClassifier::default();
    assert_codex_retry_contract(|failure| {
        RetryClassifier::classify(&classifier, failure, &openai_codex_retry_policy())
    });
}

/// Architecture v2 part 2 §2.4/§9.2; local trait-family realization of
/// `openai_codex_retry_classification_pi_exact_send`.
#[test]
fn openai_codex_retry_classification_pi_exact_local() {
    let classifier = LocalOpenAiCodexRetryClassifier::default();
    assert_codex_retry_contract(|failure| {
        LocalRetryClassifier::classify(&classifier, failure, &openai_codex_retry_policy())
    });
}

/// Architecture v2 part 2 §2.4; pinned Pi classifies the consumed HTTP error
/// text, so a terminal 429 must not be retried even when `maxRetries` is one.
#[test]
fn openai_codex_terminal_429_body_prevents_transport_retry() {
    let transport = Arc::new(TerminalLimitTransport::default());
    let api = openai_codex_responses_api(Arc::clone(&transport) as Arc<dyn HttpTransport>);
    let model = openai_codex_models().unwrap().remove(0);
    let options = SimpleGenerationOptions {
        max_retries: Some(1),
        transport: Some(StreamTransport::Sse),
        ..Default::default()
    };
    let mut policy = openai_codex_retry_policy();
    policy.max_retries = 1;
    let error = futures_executor::block_on(api.stream(
        ResolvedApiRequest {
            endpoint: model.common.base_url.clone(),
            model,
            context: Context::new(None),
            request_options: ApiRequestOptions::from(&options),
            options,
            full_options: None,
            headers: HeaderMap::new(),
            auth_headers: HeaderMap::new(),
            api_key: None,
            api: ApiId::new(OpenAiCodexResponses::API_ID),
            payload_transforms: Arc::from([]),
            response_observers: Arc::from([]),
            attempt_middleware: Arc::from([]),
            retry_policy: policy,
            timeout: None,
            retry_classifier: Arc::new(OpenAiCodexRetryClassifier::default()),
        },
        CancellationToken::new(),
    ))
    .expect_err("terminal account limit");
    assert_eq!(error.status, Some(429));
    assert!(!error.retryable);
    assert_eq!(transport.attempts.load(Ordering::SeqCst), 1);
}

/// Architecture v2 part 2 §2.4/§9.2; pinned Pi basis:
/// `openai-codex-responses.ts:415-451,1549-1574` applies the raw-body
/// allowlist first, then parses `error.message` before the catch-path
/// case-sensitive usage-limit exclusion. Raw JSON outside `error.message`
/// must neither suppress a retry nor leak as the final public error.
#[test]
fn openai_codex_retry_parses_error_before_catch_path_send_and_local() {
    let send_transport = Arc::new(ParsedErrorTransport::default());
    let send_api = HttpChatApi::new(
        Arc::new(OpenAiCodexResponsesHandler::default()),
        Arc::new(OpenAiCodexResponsesTransport::new(send_transport.clone())),
    )
    .with_retry_sleeper(Arc::new(NoopRetrySleeper));
    let send_error = futures_executor::block_on(
        send_api.stream(send_parsed_error_request(), CancellationToken::new()),
    )
    .expect_err("Send parsed Codex failure");
    assert_eq!(send_transport.attempts.load(Ordering::SeqCst), 2);
    assert!(send_error.message.ends_with("temporary"));
    assert!(!send_error.message.contains(r#"\"note\":\"usage limit\""#));

    let local_transport = Rc::new(LocalParsedErrorTransport::default());
    let local_api = LocalHttpChatApi::new(
        Rc::new(OpenAiCodexResponsesHandler::default()),
        Rc::new(LocalOpenAiCodexResponsesTransport::new(
            local_transport.clone(),
        )),
    )
    .with_retry_sleeper(Rc::new(LocalNoopRetrySleeper));
    let local_error = futures_executor::block_on(
        local_api.stream(local_parsed_error_request(), CancellationToken::new()),
    )
    .expect_err("local parsed Codex failure");
    assert_eq!(local_transport.attempts.get(), 2);
    assert!(local_error.message.ends_with("temporary"));
    assert!(!local_error.message.contains(r#"\"note\":\"usage limit\""#));
}

fn assert_codex_retry_contract(classify: impl Fn(&AttemptFailure) -> RetryDecision) {
    assert_eq!(
        classify(&http_failure(409, "conflict", HeaderMap::new())),
        RetryDecision::RetryAfter(Duration::from_secs(1)),
        "Codex's surrounding catch retries parsed 409 errors"
    );
    assert_eq!(
        classify(&http_failure(418, "teapot", HeaderMap::new())),
        RetryDecision::RetryAfter(Duration::from_secs(1)),
        "Codex's surrounding catch retries every other parsed HTTP error"
    );
    assert_eq!(
        classify(&http_failure(
            429,
            r#"{"error":{"code":"insufficient_quota","message":"billing required"}}"#,
            HeaderMap::new(),
        )),
        RetryDecision::DoNotRetry,
        "terminal account limits must not retry"
    );
    assert_eq!(
        classify(&http_failure(503, "unavailable", HeaderMap::new())),
        RetryDecision::RetryAfter(Duration::from_secs(1))
    );
    assert_eq!(
        classify(&http_failure(
            400,
            "upstream_connect_error: connection refused",
            HeaderMap::new(),
        )),
        RetryDecision::RetryAfter(Duration::from_secs(1)),
        "Codex retries its transient error-text allowlist even on another status"
    );
    let mut delayed = HeaderMap::new();
    delayed.insert("retry-after-ms", HeaderValue::from_static("250"));
    assert_eq!(
        classify(&http_failure(429, "rate limit", delayed)),
        RetryDecision::RetryAfter(Duration::from_millis(250))
    );
    assert_eq!(
        classify(&AttemptFailure::transport(
            0,
            TransportError::new("network", "monthly usage limit reached"),
        )),
        RetryDecision::DoNotRetry,
        "usage-limit failures are terminal even when surfaced as network errors"
    );
    assert_eq!(
        classify(&http_failure(400, "plain usage limit", HeaderMap::new())),
        RetryDecision::DoNotRetry,
        "the catch-path usage-limit exclusion is case-sensitive"
    );
    assert_eq!(
        classify(&http_failure(400, "plain Usage limit", HeaderMap::new())),
        RetryDecision::RetryAfter(Duration::from_secs(1)),
        "a differently cased message remains retryable like pinned Pi"
    );

    let mut ignored_delay = HeaderMap::new();
    ignored_delay.insert("retry-after-ms", HeaderValue::from_static("250"));
    assert_eq!(
        classify(&http_failure(409, "conflict", ignored_delay)),
        RetryDecision::RetryAfter(Duration::from_secs(1)),
        "catch-path HTTP retries do not consult response delay headers"
    );
}

fn http_failure(status: u16, message: &str, headers: HeaderMap) -> AttemptFailure {
    AttemptFailure::http(0, status, headers, message)
}

#[derive(Default)]
struct TerminalLimitTransport {
    attempts: AtomicUsize,
    requests: Mutex<Vec<HttpRequest>>,
}

#[derive(Default)]
struct ParsedErrorTransport {
    attempts: AtomicUsize,
}

impl HttpTransport for ParsedErrorTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                400,
                HeaderMap::new(),
                br#"{"note":"usage limit","error":{"message":"temporary"}}"#.to_vec(),
            ))
        })
    }
}

#[derive(Default)]
struct LocalParsedErrorTransport {
    attempts: Cell<usize>,
}

impl LocalHttpTransport for LocalParsedErrorTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.attempts.set(self.attempts.get() + 1);
        Box::pin(async {
            Ok(LocalHttpResponse::from_bytes(
                400,
                HeaderMap::new(),
                br#"{"note":"usage limit","error":{"message":"temporary"}}"#.to_vec(),
            ))
        })
    }
}

struct NoopRetrySleeper;

impl RetrySleeper for NoopRetrySleeper {
    fn sleep(
        &self,
        _duration: Duration,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AttemptFailure>> {
        Box::pin(async { Ok(()) })
    }
}

struct LocalNoopRetrySleeper;

impl LocalRetrySleeper for LocalNoopRetrySleeper {
    fn sleep(
        &self,
        _duration: Duration,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AttemptFailure>> {
        Box::pin(async { Ok(()) })
    }
}

fn send_parsed_error_request() -> ResolvedApiRequest {
    let mut request = send_terminal_limit_request();
    request.retry_policy.max_retries = 1;
    request
}

fn local_parsed_error_request() -> LocalResolvedApiRequest {
    let send = send_parsed_error_request();
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
        retry_classifier: Rc::new(LocalOpenAiCodexRetryClassifier::default()),
    }
}

fn send_terminal_limit_request() -> ResolvedApiRequest {
    let model = openai_codex_models().unwrap().remove(0);
    let options = SimpleGenerationOptions {
        max_retries: Some(1),
        transport: Some(StreamTransport::Sse),
        ..Default::default()
    };
    let mut policy = openai_codex_retry_policy();
    policy.max_retries = 1;
    ResolvedApiRequest {
        endpoint: model.common.base_url.clone(),
        model,
        context: Context::new(None),
        request_options: ApiRequestOptions::from(&options),
        options,
        full_options: None,
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: ApiId::new(OpenAiCodexResponses::API_ID),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: policy,
        timeout: None,
        retry_classifier: Arc::new(OpenAiCodexRetryClassifier::default()),
    }
}

impl HttpTransport for TerminalLimitTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(HttpResponse::from_bytes(
                429,
                HeaderMap::new(),
                br#"{"error":{"code":"insufficient_quota","message":"billing required"}}"#.to_vec(),
            ))
        })
    }
}
