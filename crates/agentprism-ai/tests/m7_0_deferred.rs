//! M7.0 deferred-response conformance against pinned Pi
//! `packages/ai/test/providers.test.ts`.

use agentprism_ai::*;
use futures_executor::block_on;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue};
use serde_json::{json, value::RawValue};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use url::Url;

const PROVIDER: &str = "deferred-provider";
const MODEL: &str = "deferred-model";
const API: &str = "deferred-api";
const PROVIDERS_BASIS: &str = "packages/ai/test/providers.test.ts:344-367,426-506,574-628; packages/ai/src/providers/faux.ts:500-640";
const AUTH_DEFERRED_BASIS: &str =
    "packages/ai/test/providers.test.ts:426-506; packages/ai/src/models.ts:641-732";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn header_spec(values: &[(&str, Option<&str>)]) -> HeaderMapSpec {
    values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.map(str::to_owned)))
        .collect::<BTreeMap<_, _>>()
}

fn model(provider: &str, model: &str, api: &str) -> ModelDescriptor {
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, model),
            display_name: model.into(),
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
            headers: header_spec(&[("x-model", Some("model")), ("x-shared", Some("model"))]),
        },
        api: ApiModelConfig::Custom(CustomApiModelConfig {
            api: ApiId::new(api),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

fn handle() -> DeferredHandle {
    DeferredHandle {
        schema_version: DEFERRED_HANDLE_SCHEMA_VERSION,
        provider: ProviderId::new(PROVIDER),
        model_id: ModelId::new(MODEL),
        api: ApiId::new(API),
        id: "response-1".into(),
        expires_at: Some(Timestamp::from_unix_millis(1_800_000_000_000)),
        poll_after_ms: Some(25),
        data: Some(json!({"batch_id":"batch-7","row":3})),
    }
}

fn deferred_request() -> ModelRequest {
    ModelRequest {
        model: ModelRef::new(PROVIDER, MODEL),
        context: agentprism_ai::Context::new(None),
        options: SimpleGenerationOptions {
            deferred: Some(DeferredSubmission::Window {
                window: Some(DeferredWindow::OneHour),
            }),
            ..SimpleGenerationOptions::default()
        },
    }
}

fn replay_response() -> ScriptedResponse {
    text_response("ready")
        .with_api(API)
        .with_replay_item(ScriptedReplayItem {
            id: ReplayItemId::new("replay-1"),
            ordinal: 0,
            target: ScriptedReplayTarget::Message,
            kind: ReplayKind::new("provider.deferred.fixture"),
            applicability: ReplayApplicability::ExactProviderApiModel,
            payload: OpaquePayload::Utf8("opaque-replay".into()),
        })
}

async fn terminal_send(runtime: &ScriptedRuntime, request: ModelRequest) -> AssistantMessage {
    let events = ModelRuntime::stream(runtime, request, CancellationToken::new())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .cloned()
        .expect("scripted response terminates")
}

async fn terminal_local(runtime: &ScriptedRuntime, request: ModelRequest) -> AssistantMessage {
    let events = LocalModelRuntime::stream(runtime, request, CancellationToken::new())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .cloned()
        .expect("local scripted response terminates")
}

#[test]
fn deferred_handle_persistence_round_trip_pi_exact() {
    let _basis = PROVIDERS_BASIS;
    let source = handle();
    let encoded = serde_json::to_string(&source).unwrap();
    assert_eq!(
        encoded,
        r#"{"schema_version":1,"provider":"deferred-provider","model_id":"deferred-model","api":"deferred-api","id":"response-1","expires_at":1800000000000,"poll_after_ms":25,"data":{"batch_id":"batch-7","row":3}}"#
    );
    assert_eq!(
        serde_json::from_str::<DeferredHandle>(&encoded).unwrap(),
        source
    );
    assert_eq!(
        serde_json::to_value(DeferredSubmission::Enabled).unwrap(),
        json!(true)
    );
    assert_eq!(
        serde_json::to_value(DeferredSubmission::Disabled).unwrap(),
        json!(false)
    );
    assert_eq!(
        serde_json::to_value(DeferredSubmission::Window { window: None }).unwrap(),
        json!({})
    );
    assert_eq!(
        serde_json::to_value(DeferredSubmission::Window {
            window: Some(DeferredWindow::OneHour),
        })
        .unwrap(),
        json!({"window":"1h"})
    );
}

#[test]
fn deferred_scripted_submit_poll_resolve_replay_send_and_local() {
    let _basis = PROVIDERS_BASIS;
    block_on(async {
        let send = ScriptedRuntime::builder()
            .response(deferred_response(handle(), replay_response()).with_pending_fetches(1))
            .build();
        let submitted = terminal_send(&send, deferred_request()).await;
        assert_eq!(submitted.finish.reason, AssistantFinishReason::Deferred);
        assert!(submitted.content.is_empty());

        let persisted = serde_json::to_vec(&submitted).unwrap();
        let restored: AssistantMessage = serde_json::from_slice(&persisted).unwrap();
        let restored_handle = restored.deferred.expect("deferred handle persists");
        assert_eq!(restored_handle, handle());

        let pending = DeferredModelRuntime::fetch_deferred(
            &send,
            ModelRef::new(PROVIDER, MODEL),
            restored_handle.clone(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(pending.finish.reason, AssistantFinishReason::Deferred);
        assert_eq!(pending.deferred.as_ref(), Some(&restored_handle));

        let ready = DeferredModelRuntime::fetch_deferred(
            &send,
            ModelRef::new(PROVIDER, MODEL),
            restored_handle,
            DeferredFetchOptions {
                wait_ms: Some(0),
                ..DeferredFetchOptions::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(ready.finish.reason, AssistantFinishReason::Stop);
        assert_eq!(ready.replay.items.len(), 1);
        assert_eq!(ready.replay.items[0].as_utf8(), Some("opaque-replay"));
        let persisted_ready = serde_json::to_vec(&ready).unwrap();
        assert_eq!(
            serde_json::from_slice::<AssistantMessage>(&persisted_ready).unwrap(),
            ready
        );
        assert_eq!(send.deferred_fetch_count(), 2);

        let local = ScriptedRuntime::builder()
            .response(deferred_response(handle(), replay_response()).with_pending_fetches(1))
            .build();
        let local_submitted = terminal_local(&local, deferred_request()).await;
        let local_persisted = serde_json::to_vec(&local_submitted).unwrap();
        let local_restored: AssistantMessage = serde_json::from_slice(&local_persisted).unwrap();
        let local_handle = local_restored.deferred.expect("local handle persists");

        let local_pending = LocalDeferredModelRuntime::fetch_deferred(
            &local,
            ModelRef::new(PROVIDER, MODEL),
            local_handle.clone(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(local_pending.finish.reason, AssistantFinishReason::Deferred);
        assert_eq!(local_pending.deferred.as_ref(), Some(&local_handle));

        let local_ready = LocalDeferredModelRuntime::fetch_deferred(
            &local,
            ModelRef::new(PROVIDER, MODEL),
            local_handle,
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(local_ready.finish.reason, AssistantFinishReason::Stop);
        assert_eq!(local_ready.replay.items.len(), 1);
        assert_eq!(local_ready.replay.items[0].as_utf8(), Some("opaque-replay"));
        assert_eq!(local.deferred_fetch_count(), 2);
    });
}

fn deferred_failure_response() -> ScriptedResponse {
    ScriptedResponse::failure(PublicError {
        code: "deferred_failed".into(),
        message: "deferred failed".into(),
        retryable: false,
        provider_code: None,
        status: None,
        request_id: None,
    })
    .with_api(API)
}

#[test]
fn deferred_scripted_failure_and_cancel_send_and_local() {
    let _basis = PROVIDERS_BASIS;
    block_on(async {
        let send_failure = ScriptedRuntime::builder()
            .deferred(handle(), deferred_failure_response())
            .build();
        let failed_submission = terminal_send(&send_failure, deferred_request()).await;
        let failed = DeferredModelRuntime::fetch_deferred(
            &send_failure,
            ModelRef::new(PROVIDER, MODEL),
            failed_submission.deferred.unwrap(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(failed.finish.reason, AssistantFinishReason::Error);
        assert_eq!(
            failed
                .finish
                .error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some("deferred failed")
        );

        let send = ScriptedRuntime::builder()
            .deferred(handle(), text_response("unreachable").with_api(API))
            .build();
        let submitted = terminal_send(&send, deferred_request()).await;
        let submitted_handle = submitted.deferred.unwrap();
        DeferredModelRuntime::cancel_deferred(
            &send,
            ModelRef::new(PROVIDER, MODEL),
            submitted_handle.clone(),
            DeferredCancelOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let failed = DeferredModelRuntime::fetch_deferred(
            &send,
            ModelRef::new(PROVIDER, MODEL),
            submitted_handle.clone(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(failed.finish.reason, AssistantFinishReason::Error);
        assert_eq!(
            failed
                .finish
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("deferred_cancelled")
        );
        assert_eq!(send.cancelled_deferred(), vec![submitted_handle]);

        let local_failure = ScriptedRuntime::builder()
            .deferred(handle(), deferred_failure_response())
            .build();
        let local_failed_submission = terminal_local(&local_failure, deferred_request()).await;
        let local_failed = LocalDeferredModelRuntime::fetch_deferred(
            &local_failure,
            ModelRef::new(PROVIDER, MODEL),
            local_failed_submission.deferred.unwrap(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(local_failed.finish.reason, AssistantFinishReason::Error);
        assert_eq!(
            local_failed
                .finish
                .error
                .as_ref()
                .map(|error| error.message.as_str()),
            Some("deferred failed")
        );

        let local = ScriptedRuntime::builder()
            .deferred(handle(), text_response("unreachable").with_api(API))
            .build();
        let local_submitted = terminal_local(&local, deferred_request()).await;
        let local_handle = local_submitted.deferred.unwrap();
        LocalDeferredModelRuntime::cancel_deferred(
            &local,
            ModelRef::new(PROVIDER, MODEL),
            local_handle.clone(),
            DeferredCancelOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let local_failed = LocalDeferredModelRuntime::fetch_deferred(
            &local,
            ModelRef::new(PROVIDER, MODEL),
            local_handle.clone(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(local_failed.finish.reason, AssistantFinishReason::Error);
        assert_eq!(
            local_failed
                .finish
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("deferred_cancelled")
        );
        assert_eq!(local.cancelled_deferred(), vec![local_handle]);
    });
}

#[derive(Clone, Debug)]
struct CapturedDeferredRequest {
    operation: &'static str,
    handle: DeferredHandle,
    wait_ms: Option<u64>,
    request_options: ApiRequestOptions,
    endpoint: Url,
    headers: HeaderMap,
    api_key: Option<String>,
    max_retries: u32,
    timeout: Option<Duration>,
}

#[derive(Default)]
struct RecordingDeferredApi {
    captured: Mutex<Vec<CapturedDeferredRequest>>,
}

impl RecordingDeferredApi {
    fn capture(
        &self,
        operation: &'static str,
        request: &ResolvedDeferredRequest,
    ) -> CapturedDeferredRequest {
        CapturedDeferredRequest {
            operation,
            handle: request.handle.clone(),
            wait_ms: request.wait_ms,
            request_options: request.request_options.clone(),
            endpoint: request.endpoint.clone(),
            headers: request.headers.clone(),
            api_key: request
                .api_key
                .as_ref()
                .map(|secret| secret.expose_secret().to_owned()),
            max_retries: request.retry_policy.max_retries,
            timeout: request.timeout,
        }
    }
}

impl ChatApi for RecordingDeferredApi {
    fn deferred_capabilities(&self) -> DeferredCapabilities {
        DeferredCapabilities::FETCH_AND_CANCEL
    }

    fn fetch_deferred(
        &self,
        request: ResolvedDeferredRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        let captured = self.capture("fetch", &request);
        lock(&self.captured).push(captured);
        Box::pin(async move {
            let model_ref = request.model.common.model_ref.clone();
            ModelRuntime::stream(
                &ScriptedRuntime::builder()
                    .response(text_response("routed").with_api(request.api))
                    .build(),
                ModelRequest {
                    model: model_ref.clone(),
                    context: agentprism_ai::Context::new(None),
                    options: SimpleGenerationOptions::default(),
                },
                cancellation,
            )
            .await
            .map_err(|error| {
                AiError::new(AiErrorKind::Internal, error.message).with_model(model_ref)
            })
        })
    }

    fn cancel_deferred(
        &self,
        request: ResolvedDeferredRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AiError>> {
        let captured = self.capture("cancel", &request);
        lock(&self.captured).push(captured);
        Box::pin(async { Ok(()) })
    }

    fn stream(
        &self,
        _request: ResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(async { Ok(AssistantStream::new(futures_util::stream::empty())) })
    }
}

#[derive(Default)]
struct LocalRecordingDeferredApi {
    captured: RefCell<Vec<CapturedDeferredRequest>>,
}

impl LocalRecordingDeferredApi {
    fn capture(
        &self,
        operation: &'static str,
        request: &LocalResolvedDeferredRequest,
    ) -> CapturedDeferredRequest {
        CapturedDeferredRequest {
            operation,
            handle: request.handle.clone(),
            wait_ms: request.wait_ms,
            request_options: request.request_options.clone(),
            endpoint: request.endpoint.clone(),
            headers: request.headers.clone(),
            api_key: request
                .api_key
                .as_ref()
                .map(|secret| secret.expose_secret().to_owned()),
            max_retries: request.retry_policy.max_retries,
            timeout: request.timeout,
        }
    }
}

impl LocalChatApi for LocalRecordingDeferredApi {
    fn deferred_capabilities(&self) -> DeferredCapabilities {
        DeferredCapabilities::FETCH_AND_CANCEL
    }

    fn fetch_deferred(
        &self,
        request: LocalResolvedDeferredRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        let captured = self.capture("fetch", &request);
        self.captured.borrow_mut().push(captured);
        Box::pin(async move {
            let model_ref = request.model.common.model_ref.clone();
            LocalModelRuntime::stream(
                &ScriptedRuntime::builder()
                    .response(text_response("routed").with_api(request.api))
                    .build(),
                ModelRequest {
                    model: model_ref.clone(),
                    context: agentprism_ai::Context::new(None),
                    options: SimpleGenerationOptions::default(),
                },
                cancellation,
            )
            .await
            .map_err(|error| {
                AiError::new(AiErrorKind::Internal, error.message).with_model(model_ref)
            })
        })
    }

    fn cancel_deferred(
        &self,
        request: LocalResolvedDeferredRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AiError>> {
        let captured = self.capture("cancel", &request);
        self.captured.borrow_mut().push(captured);
        Box::pin(async { Ok(()) })
    }

    fn stream(
        &self,
        _request: LocalResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        Box::pin(async { Ok(LocalAssistantStream::new(futures_util::stream::empty())) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedAuthOverrides {
    api_key: Option<String>,
    environment: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct DeferredAuth {
    captured: Arc<Mutex<Vec<CapturedAuthOverrides>>>,
}

impl DeferredAuth {
    fn capture(&self, overrides: &AuthResolutionOverrides) -> ResolvedAuth {
        let mut environment = BTreeMap::from([
            ("PROVIDER_ONLY".to_owned(), "provider".to_owned()),
            ("SHARED".to_owned(), "provider".to_owned()),
        ]);
        environment.extend(overrides.environment.clone());
        lock(&self.captured).push(CapturedAuthOverrides {
            api_key: overrides
                .api_key
                .as_ref()
                .map(|secret| secret.expose_secret().to_owned()),
            environment,
        });

        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer provider"));
        headers.insert("x-shared", HeaderValue::from_static("auth"));
        ResolvedAuth {
            api_key: overrides
                .api_key
                .clone()
                .or_else(|| Some(SecretString::new("provider-secret"))),
            headers,
            transport_headers: HeaderMap::new(),
            base_url: Some(Url::parse("https://resolved.example/v1").unwrap()),
            source: AuthSource::new("fixture"),
        }
    }
}

impl AuthResolver for DeferredAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let resolved = self.capture(&request.overrides);
        Box::pin(async move { Ok(Some(resolved)) })
    }
}

impl LocalAuthResolver for DeferredAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let resolved = self.capture(&request.overrides);
        Box::pin(async move { Ok(Some(resolved)) })
    }
}

struct DeferredHeaderTransform;

impl HeaderTransform for DeferredHeaderTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            headers.insert("x-transformed", HeaderValue::from_static("yes"));
            Ok(())
        })
    }
}

impl LocalHeaderTransform for DeferredHeaderTransform {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        Box::pin(async move {
            headers.insert("x-transformed", HeaderValue::from_static("yes"));
            Ok(())
        })
    }
}

fn fetch_options() -> DeferredFetchOptions {
    DeferredFetchOptions {
        wait_ms: Some(50),
        request: ApiRequestOptions {
            max_retries: Some(4),
            timeout_ms: Some(100),
            session_id: Some("session-1".into()),
            headers: header_spec(&[
                ("x-request", Some("request")),
                ("x-shared", Some("request")),
            ]),
            ..ApiRequestOptions::default()
        },
    }
}

fn cancel_options() -> DeferredCancelOptions {
    ApiRequestOptions {
        timeout_ms: Some(200),
        headers: header_spec(&[("x-cancel", Some("yes"))]),
        ..ApiRequestOptions::default()
    }
}

fn fetch_auth_overrides() -> AuthResolutionOverrides {
    AuthResolutionOverrides {
        api_key: Some(SecretString::new("request-fetch-key")),
        environment: BTreeMap::from([
            ("REQUEST_ONLY".to_owned(), "request".to_owned()),
            ("SHARED".to_owned(), "request".to_owned()),
        ]),
        min_oauth_validity: None,
    }
}

fn cancel_auth_overrides() -> AuthResolutionOverrides {
    AuthResolutionOverrides {
        api_key: Some(SecretString::new("request-cancel-key")),
        environment: BTreeMap::from([
            ("CANCEL_ONLY".to_owned(), "cancel".to_owned()),
            ("SHARED".to_owned(), "cancel".to_owned()),
        ]),
        min_oauth_validity: None,
    }
}

fn assert_auth_captured(captured: &[CapturedAuthOverrides]) {
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0],
        CapturedAuthOverrides {
            api_key: Some("request-fetch-key".into()),
            environment: BTreeMap::from([
                ("PROVIDER_ONLY".to_owned(), "provider".to_owned()),
                ("REQUEST_ONLY".to_owned(), "request".to_owned()),
                ("SHARED".to_owned(), "request".to_owned()),
            ]),
        }
    );
    assert_eq!(
        captured[1],
        CapturedAuthOverrides {
            api_key: Some("request-cancel-key".into()),
            environment: BTreeMap::from([
                ("CANCEL_ONLY".to_owned(), "cancel".to_owned()),
                ("PROVIDER_ONLY".to_owned(), "provider".to_owned()),
                ("SHARED".to_owned(), "cancel".to_owned()),
            ]),
        }
    );
}

fn assert_captured(captured: &[CapturedDeferredRequest]) {
    assert_eq!(captured.len(), 2);
    let fetched = &captured[0];
    assert_eq!(fetched.operation, "fetch");
    assert_eq!(fetched.handle, handle());
    assert_eq!(fetched.wait_ms, Some(50));
    assert_eq!(fetched.endpoint.as_str(), "https://resolved.example/v1");
    assert_eq!(fetched.headers["authorization"], "Bearer provider");
    assert_eq!(fetched.headers["x-model"], "model");
    assert_eq!(fetched.headers["x-request"], "request");
    assert_eq!(fetched.headers["x-shared"], "request");
    assert_eq!(fetched.headers["x-transformed"], "yes");
    assert_eq!(fetched.api_key.as_deref(), Some("request-fetch-key"));
    assert_eq!(fetched.max_retries, 4);
    assert_eq!(fetched.timeout, Some(Duration::from_millis(100)));
    assert_eq!(
        fetched.request_options.session_id.as_deref(),
        Some("session-1")
    );

    let cancelled = &captured[1];
    assert_eq!(cancelled.operation, "cancel");
    assert_eq!(cancelled.handle, handle());
    assert_eq!(cancelled.wait_ms, None);
    assert_eq!(cancelled.headers["authorization"], "Bearer provider");
    assert_eq!(cancelled.headers["x-model"], "model");
    assert_eq!(cancelled.headers["x-cancel"], "yes");
    assert_eq!(cancelled.headers["x-transformed"], "yes");
    assert_eq!(cancelled.api_key.as_deref(), Some("request-cancel-key"));
    assert_eq!(cancelled.timeout, Some(Duration::from_millis(200)));
}

#[test]
fn deferred_models_request_options_reach_handler_send_and_local() {
    let _basis = (PROVIDERS_BASIS, AUTH_DEFERRED_BASIS);
    block_on(async {
        let send_api = Arc::new(RecordingDeferredApi::default());
        let send_auth = DeferredAuth::default();
        let send_registration = ProviderRegistration::builder(PROVIDER)
            .headers(header_spec(&[("x-provider", Some("provider"))]))
            .auth(Arc::new(send_auth.clone()))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), send_api.clone())
            .build()
            .unwrap();
        let send_models = Models::builder()
            .provider(send_registration)
            .header_transform(Arc::new(DeferredHeaderTransform))
            .build()
            .unwrap();
        let ready = send_models
            .fetch_deferred_with_auth(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                fetch_options(),
                fetch_auth_overrides(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(ready.finish.reason, AssistantFinishReason::Stop);
        send_models
            .cancel_deferred_with_auth(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                cancel_options(),
                cancel_auth_overrides(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_captured(&lock(&send_api.captured));
        assert_auth_captured(&lock(&send_auth.captured));

        let local_api = Rc::new(LocalRecordingDeferredApi::default());
        let local_auth = DeferredAuth::default();
        let local_registration = LocalProviderRegistration::builder(PROVIDER)
            .headers(header_spec(&[("x-provider", Some("provider"))]))
            .auth(Rc::new(local_auth.clone()))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), local_api.clone())
            .build()
            .unwrap();
        let local_models = LocalModels::builder()
            .provider(local_registration)
            .header_transform(Rc::new(DeferredHeaderTransform))
            .build()
            .unwrap();
        let local_ready = local_models
            .fetch_deferred_with_auth(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                fetch_options(),
                fetch_auth_overrides(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(local_ready.finish.reason, AssistantFinishReason::Stop);
        local_models
            .cancel_deferred_with_auth(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                cancel_options(),
                cancel_auth_overrides(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_captured(&local_api.captured.borrow());
        assert_auth_captured(&lock(&local_auth.captured));
    });
}

struct UnsupportedApi;

impl ChatApi for UnsupportedApi {
    fn stream(
        &self,
        _request: ResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(async { Ok(AssistantStream::new(futures_util::stream::empty())) })
    }
}

impl LocalChatApi for UnsupportedApi {
    fn stream(
        &self,
        _request: LocalResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        Box::pin(async { Ok(LocalAssistantStream::new(futures_util::stream::empty())) })
    }
}

struct CountingAuth {
    calls: Arc<AtomicUsize>,
}

impl AuthResolver for CountingAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    }
}

impl LocalAuthResolver for CountingAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    }
}

#[test]
fn deferred_unsupported_provider_precedes_auth_send_and_local() {
    let _basis = PROVIDERS_BASIS;
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = ProviderRegistration::builder(PROVIDER)
            .auth(Arc::new(CountingAuth {
                calls: Arc::clone(&calls),
            }))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Arc::new(UnsupportedApi))
            .build()
            .unwrap();
        let models = Models::builder().provider(registration).build().unwrap();
        let fetch_error = DeferredModelRuntime::fetch_deferred(
            &models,
            ModelRef::new(PROVIDER, MODEL),
            handle(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            fetch_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            fetch_error.message,
            "Provider deferred-provider does not support deferred responses"
        );
        let cancel_error = DeferredModelRuntime::cancel_deferred(
            &models,
            ModelRef::new(PROVIDER, MODEL),
            handle(),
            DeferredCancelOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            cancel_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let local_registration = LocalProviderRegistration::builder(PROVIDER)
            .auth(Rc::new(CountingAuth {
                calls: Arc::clone(&calls),
            }))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Rc::new(UnsupportedApi))
            .build()
            .unwrap();
        let local_models = LocalModels::builder()
            .provider(local_registration)
            .build()
            .unwrap();
        let local_fetch_error = LocalDeferredModelRuntime::fetch_deferred(
            &local_models,
            ModelRef::new(PROVIDER, MODEL),
            handle(),
            DeferredFetchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            local_fetch_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            local_fetch_error.message,
            "Provider deferred-provider does not support deferred responses"
        );
        let local_cancel_error = LocalDeferredModelRuntime::cancel_deferred(
            &local_models,
            ModelRef::new(PROVIDER, MODEL),
            handle(),
            DeferredCancelOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            local_cancel_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn deferred_mixed_api_capability_check_follows_auth_send_and_local() {
    let _basis = "packages/ai/src/models.ts:706-732,834-858";
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = ProviderRegistration::builder(PROVIDER)
            .auth(Arc::new(CountingAuth {
                calls: Arc::clone(&calls),
            }))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Arc::new(UnsupportedApi))
            .api(
                ApiId::new("deferred-capable-api"),
                Arc::new(RecordingDeferredApi::default()),
            )
            .build()
            .unwrap();
        let models = Models::builder().provider(registration).build().unwrap();
        let error = models
            .fetch_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredFetchOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, RequestStartErrorKind::RuntimeUnavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let local_registration = LocalProviderRegistration::builder(PROVIDER)
            .auth(Rc::new(CountingAuth {
                calls: Arc::clone(&calls),
            }))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Rc::new(UnsupportedApi))
            .api(
                ApiId::new("deferred-capable-api"),
                Rc::new(LocalRecordingDeferredApi::default()),
            )
            .build()
            .unwrap();
        let local_models = LocalModels::builder()
            .provider(local_registration)
            .build()
            .unwrap();
        let error = local_models
            .fetch_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredFetchOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, RequestStartErrorKind::RuntimeUnavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let send_auth = DeferredAuth::default();
        let registration = ProviderRegistration::builder(PROVIDER)
            .auth(Arc::new(send_auth.clone()))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Arc::new(UnsupportedApi))
            .api(
                ApiId::new("deferred-capable-api"),
                Arc::new(RecordingDeferredApi::default()),
            )
            .build()
            .unwrap();
        let models = Models::builder().provider(registration).build().unwrap();
        let fetch_error = models
            .fetch_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredFetchOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            fetch_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            fetch_error.message,
            "Provider deferred-provider does not support deferred responses for \"deferred-api\""
        );
        let cancel_error = models
            .cancel_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredCancelOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            cancel_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            cancel_error.message,
            "Provider deferred-provider cannot cancel deferred responses for \"deferred-api\""
        );
        assert_eq!(lock(&send_auth.captured).len(), 2);

        let local_auth = DeferredAuth::default();
        let local_registration = LocalProviderRegistration::builder(PROVIDER)
            .auth(Rc::new(local_auth.clone()))
            .models(vec![model(PROVIDER, MODEL, API)])
            .api(ApiId::new(API), Rc::new(UnsupportedApi))
            .api(
                ApiId::new("deferred-capable-api"),
                Rc::new(LocalRecordingDeferredApi::default()),
            )
            .build()
            .unwrap();
        let local_models = LocalModels::builder()
            .provider(local_registration)
            .build()
            .unwrap();
        let fetch_error = local_models
            .fetch_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredFetchOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            fetch_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            fetch_error.message,
            "Provider deferred-provider does not support deferred responses for \"deferred-api\""
        );
        let cancel_error = local_models
            .cancel_deferred(
                ModelRef::new(PROVIDER, MODEL),
                handle(),
                DeferredCancelOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            cancel_error.kind,
            RequestStartErrorKind::UnsupportedOperation
        );
        assert_eq!(
            cancel_error.message,
            "Provider deferred-provider cannot cancel deferred responses for \"deferred-api\""
        );
        assert_eq!(lock(&local_auth.captured).len(), 2);
    });
}

#[test]
fn deferred_capabilities_are_optional_and_independent_send_and_local() {
    let _basis = PROVIDERS_BASIS;
    let unsupported = UnsupportedApi;
    assert_eq!(
        ChatApi::deferred_capabilities(&unsupported),
        DeferredCapabilities::NONE
    );
    assert_eq!(
        LocalChatApi::deferred_capabilities(&unsupported),
        DeferredCapabilities::NONE
    );

    let send = RecordingDeferredApi::default();
    assert_eq!(
        ChatApi::deferred_capabilities(&send),
        DeferredCapabilities::FETCH_AND_CANCEL
    );
    let local = LocalRecordingDeferredApi::default();
    assert_eq!(
        LocalChatApi::deferred_capabilities(&local),
        DeferredCapabilities::FETCH_AND_CANCEL
    );

    let fetch_only = DeferredCapabilities::FETCH;
    assert!(fetch_only.fetch);
    assert!(!fetch_only.cancel);

    let runtime = ScriptedRuntime::builder().build();
    let _send_object: &dyn DeferredModelRuntime = &runtime;
    let _local_object: &dyn LocalDeferredModelRuntime = &runtime;
}

#[test]
fn deferred_terminal_requires_durable_handle() {
    let _basis = "packages/ai/src/types.ts:405-440; architecture v2 part 2 §10.1 stream assembly";
    let mut assembler = AssistantAssembler::new();
    assembler
        .apply(&AssistantEvent::MessageStarted {
            message_id: MessageId::new("message-1"),
            provider: ProviderId::new(PROVIDER),
            api: ApiId::new(API),
            model: ModelId::new(MODEL),
        })
        .unwrap();
    assert_eq!(
        assembler
            .clone()
            .finish_completed(AssistantFinish {
                reason: AssistantFinishReason::Deferred,
                raw_provider_reason: None,
                error: None,
            })
            .unwrap_err(),
        AssemblyError::MissingDeferredHandle
    );
    let message = assembler.finish_deferred(handle()).unwrap();
    assert_eq!(message.finish.reason, AssistantFinishReason::Deferred);
    assert_eq!(message.deferred, Some(handle()));
}
