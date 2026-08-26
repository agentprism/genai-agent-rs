use agentprism_ai::*;
use futures_executor::block_on;
use http::HeaderMap;
use serde_json::value::RawValue;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::Duration;
use url::Url;

const PROVIDER: &str = "catalog-provider";
const API: &str = "catalog-test-api";

type FetchInspector = Arc<dyn Fn(&CatalogFetchContext) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn test_model(provider: &str, id: &str, display_name: &str) -> ModelDescriptor {
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, id),
            display_name: display_name.into(),
            base_url: Url::parse("https://catalog.example/v1").unwrap(),
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
            api: ApiId::new(API),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

fn candidate(models: Vec<ModelDescriptor>, revision: &str) -> CatalogCandidate {
    CatalogCandidate {
        models,
        checked_at: Timestamp::from_unix_millis(1_750_000_000_000),
        revision: Some(revision.into()),
        etag: Some(format!("\"{revision}\"")),
        source_metadata: ExtensionMap::new(),
    }
}

fn persisted(models: Vec<ModelDescriptor>, revision: &str) -> PersistedCatalogSnapshot {
    candidate(models, revision).to_persisted()
}

#[derive(Default)]
struct NoopApi;

impl ChatApi for NoopApi {
    fn stream(
        &self,
        _request: ResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(async { Ok(AssistantStream::new(futures_util::stream::empty())) })
    }
}

#[derive(Default)]
struct LocalNoopApi;

impl LocalChatApi for LocalNoopApi {
    fn stream(
        &self,
        _request: LocalResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        Box::pin(async { Ok(LocalAssistantStream::new(futures_util::stream::empty())) })
    }
}

struct QueueSource {
    baseline: Arc<[ModelDescriptor]>,
    results: Mutex<VecDeque<Result<CatalogCandidate, CatalogError>>>,
    inspect: Option<FetchInspector>,
}

impl QueueSource {
    fn new(
        baseline: Vec<ModelDescriptor>,
        results: Vec<Result<CatalogCandidate, CatalogError>>,
    ) -> Self {
        Self {
            baseline: Arc::from(baseline),
            results: Mutex::new(results.into()),
            inspect: None,
        }
    }

    fn inspect(mut self, inspect: FetchInspector) -> Self {
        self.inspect = Some(inspect);
        self
    }
}

impl ModelCatalogSource for QueueSource {
    fn baseline(&self) -> Arc<[ModelDescriptor]> {
        Arc::clone(&self.baseline)
    }

    fn fetch(
        &self,
        context: CatalogFetchContext,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CatalogCandidate, CatalogError>> {
        let result = lock(&self.results)
            .pop_front()
            .unwrap_or_else(|| Err(CatalogError::source("no scripted catalog candidate")));
        let inspect = self.inspect.clone();
        Box::pin(async move {
            if let Some(inspect) = inspect {
                inspect(&context);
            }
            result
        })
    }
}

struct LocalQueueSource {
    baseline: Rc<[ModelDescriptor]>,
    results: RefCell<VecDeque<Result<CatalogCandidate, CatalogError>>>,
}

impl LocalQueueSource {
    fn new(
        baseline: Vec<ModelDescriptor>,
        results: Vec<Result<CatalogCandidate, CatalogError>>,
    ) -> Self {
        Self {
            baseline: Rc::from(baseline),
            results: RefCell::new(results.into()),
        }
    }
}

impl LocalModelCatalogSource for LocalQueueSource {
    fn baseline(&self) -> Rc<[ModelDescriptor]> {
        Rc::clone(&self.baseline)
    }

    fn fetch(
        &self,
        _context: CatalogFetchContext,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CatalogCandidate, CatalogError>> {
        let result =
            self.results.borrow_mut().pop_front().unwrap_or_else(|| {
                Err(CatalogError::source("no scripted local catalog candidate"))
            });
        Box::pin(async move { result })
    }
}

struct MutableModelCatalog {
    published: Mutex<Arc<[ModelDescriptor]>>,
}

impl MutableModelCatalog {
    fn new(models: Vec<ModelDescriptor>) -> Self {
        Self {
            published: Mutex::new(Arc::from(models)),
        }
    }

    fn publish(&self, models: Vec<ModelDescriptor>) {
        *lock(&self.published) = Arc::from(models);
    }
}

impl ModelCatalog for MutableModelCatalog {
    fn snapshot(&self) -> Arc<[ModelDescriptor]> {
        Arc::clone(&lock(&self.published))
    }
}

struct LocalMutableModelCatalog {
    published: RefCell<Rc<[ModelDescriptor]>>,
}

impl LocalMutableModelCatalog {
    fn new(models: Vec<ModelDescriptor>) -> Self {
        Self {
            published: RefCell::new(Rc::from(models)),
        }
    }

    fn publish(&self, models: Vec<ModelDescriptor>) {
        *self.published.borrow_mut() = Rc::from(models);
    }
}

impl LocalModelCatalog for LocalMutableModelCatalog {
    fn snapshot(&self) -> Rc<[ModelDescriptor]> {
        Rc::clone(&self.published.borrow())
    }
}

struct BlockingSource {
    baseline: Arc<[ModelDescriptor]>,
    fresh: CatalogCandidate,
    started: std::sync::mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl ModelCatalogSource for BlockingSource {
    fn baseline(&self) -> Arc<[ModelDescriptor]> {
        Arc::clone(&self.baseline)
    }

    fn fetch(
        &self,
        _context: CatalogFetchContext,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CatalogCandidate, CatalogError>> {
        let started = self.started.clone();
        let release = Arc::clone(&self.release);
        let fresh = self.fresh.clone();
        Box::pin(async move {
            started.send(()).unwrap();
            let (mutex, condition) = &*release;
            let mut released = lock(mutex);
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(fresh)
        })
    }
}

struct BackgroundWriteState {
    completed: bool,
    waker: Option<Waker>,
}

struct BackgroundWriteCompletion {
    state: Arc<Mutex<BackgroundWriteState>>,
}

impl Future for BackgroundWriteCompletion {
    type Output = Result<(), StoreError>;

    fn poll(self: std::pin::Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock(&self.state);
        if state.completed {
            Poll::Ready(Ok(()))
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

/// Starts its first durable write on a background thread. Dropping the
/// returned future does not cancel that I/O, so this exercises the exact stale
/// durable-commit race that provider-ID publication sequencing must prevent.
struct BackgroundFirstWriteStore {
    entries: Arc<Mutex<BTreeMap<ProviderId, PersistedCatalogSnapshot>>>,
    calls: AtomicUsize,
    first_started: std::sync::mpsc::Sender<()>,
    second_started: std::sync::mpsc::Sender<()>,
    release_first: Arc<(Mutex<bool>, Condvar)>,
}

impl ModelsStore for BackgroundFirstWriteStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "catalog read cancelled"))?;
            Ok(lock(&self.entries).get(&provider).cloned())
        })
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let provider = provider.clone();
            let snapshot = snapshot.clone();
            let entries = Arc::clone(&self.entries);
            let first_started = self.first_started.clone();
            let release_first = Arc::clone(&self.release_first);
            let state = Arc::new(Mutex::new(BackgroundWriteState {
                completed: false,
                waker: None,
            }));
            let worker_state = Arc::clone(&state);
            let _worker = thread::spawn(move || {
                first_started.send(()).unwrap();
                let (mutex, condition) = &*release_first;
                let mut released = lock(mutex);
                while !*released {
                    released = condition
                        .wait(released)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                drop(released);
                lock(&entries).insert(provider, snapshot);
                let waker = {
                    let mut state = lock(&worker_state);
                    state.completed = true;
                    state.waker.take()
                };
                if let Some(waker) = waker {
                    waker.wake();
                }
            });
            return Box::pin(BackgroundWriteCompletion { state });
        }

        self.second_started.send(()).unwrap();
        let provider = provider.clone();
        let snapshot = snapshot.clone();
        Box::pin(async move {
            lock(&self.entries).insert(provider, snapshot);
            Ok(())
        })
    }

    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "catalog delete cancelled"))?;
            lock(&self.entries).remove(&provider);
            Ok(())
        })
    }
}

/// A deliberately non-cooperative store write. The first write blocks while
/// holding the caller's publication future and commits even if its token was
/// cancelled. This models a durable backend whose in-flight I/O cannot be
/// recalled, which is why publication serialization must survive delete/re-add.
struct BlockingFirstWriteStore {
    inner: InMemoryModelsStore,
    calls: AtomicUsize,
    first_started: std::sync::mpsc::Sender<()>,
    second_started: std::sync::mpsc::Sender<()>,
    release_first: Arc<(Mutex<bool>, Condvar)>,
}

impl ModelsStore for BlockingFirstWriteStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        ModelsStore::read(&self.inner, provider, cancellation)
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let provider = provider.clone();
            let snapshot = snapshot.clone();
            let first_started = self.first_started.clone();
            let release_first = Arc::clone(&self.release_first);
            return Box::pin(async move {
                first_started.send(()).unwrap();
                {
                    let (mutex, condition) = &*release_first;
                    let mut released = lock(mutex);
                    while !*released {
                        released = condition
                            .wait(released)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
                ModelsStore::write(&self.inner, &provider, &snapshot, CancellationToken::new())
                    .await
            });
        }

        self.second_started.send(()).unwrap();
        ModelsStore::write(&self.inner, provider, snapshot, CancellationToken::new())
    }

    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        ModelsStore::delete(&self.inner, provider, cancellation)
    }
}

struct LocalBlockingFirstWriteStore {
    entries: RefCell<BTreeMap<ProviderId, PersistedCatalogSnapshot>>,
    calls: Cell<usize>,
    release_first: Rc<Cell<bool>>,
}

impl LocalModelsStore for LocalBlockingFirstWriteStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "local catalog read cancelled"))?;
            Ok(self.entries.borrow().get(&provider).cloned())
        })
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        let provider = provider.clone();
        let snapshot = snapshot.clone();
        if call == 0 {
            return Box::pin(futures_util::future::poll_fn(move |context| {
                if !self.release_first.get() {
                    context.waker().wake_by_ref();
                    return Poll::Pending;
                }
                self.entries
                    .borrow_mut()
                    .insert(provider.clone(), snapshot.clone());
                Poll::Ready(Ok(()))
            }));
        }

        Box::pin(async move {
            self.entries.borrow_mut().insert(provider, snapshot);
            Ok(())
        })
    }

    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "local catalog delete cancelled"))?;
            self.entries.borrow_mut().remove(&provider);
            Ok(())
        })
    }
}

struct ProbeAuth {
    probe: Arc<dyn Fn() + Send + Sync>,
}

impl AuthResolver for ProbeAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        assert!(request.model.is_none(), "catalog auth is provider-scoped");
        assert_eq!(
            request.purpose,
            AuthResolutionPurpose::CatalogRefresh,
            "catalog refresh must not use ordinary request-auth policy"
        );
        let probe = Arc::clone(&self.probe);
        Box::pin(async move {
            probe();
            Ok(Some(ResolvedAuth {
                api_key: None,
                headers: HeaderMap::new(),
                transport_headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("catalog-test"),
            }))
        })
    }
}

#[derive(Clone, Copy)]
struct FixedCatalogAuthClock(Timestamp);

impl AuthClock for FixedCatalogAuthClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

impl LocalAuthClock for FixedCatalogAuthClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

struct CatalogOAuth {
    refreshes: Arc<AtomicUsize>,
}

impl OAuthAuth for CatalogOAuth {
    fn name(&self) -> &str {
        "catalog OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "not used by catalog conformance".into(),
            })
        })
    }

    fn refresh(
        &self,
        mut credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        credential.access = SecretString::new("unexpected-refresh");
        Box::pin(async move { Ok(credential) })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move {
            Ok(ResolvedAuth {
                api_key: Some(access),
                headers: HeaderMap::new(),
                transport_headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

struct LocalCatalogOAuth {
    refreshes: Rc<Cell<usize>>,
}

impl LocalOAuthAuth for LocalCatalogOAuth {
    fn name(&self) -> &str {
        "local catalog OAuth"
    }

    fn login(
        &self,
        _interaction: Rc<dyn LocalAuthInteraction>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "not used by local catalog conformance".into(),
            })
        })
    }

    fn refresh(
        &self,
        mut credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        self.refreshes.set(self.refreshes.get() + 1);
        credential.access = SecretString::new("unexpected-local-refresh");
        Box::pin(async move { Ok(credential) })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move {
            Ok(ResolvedAuth {
                api_key: Some(access),
                headers: HeaderMap::new(),
                transport_headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

fn catalog_oauth_credential(access: &str, expires_at: i64) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new("catalog-refresh-secret"),
        expires_at: Timestamp::from_unix_millis(expires_at),
        extra: ProviderOAuthExtra::None,
    })
}

fn dynamic_provider(
    provider: &str,
    source: Arc<dyn ModelCatalogSource>,
    auth: Arc<dyn AuthResolver>,
) -> ProviderRegistration {
    ProviderRegistration::builder(provider)
        .auth(auth)
        .catalog_source(source)
        .api(ApiId::new(API), Arc::new(NoopApi))
        .build()
        .unwrap()
}

fn static_provider(provider: &str, models: Vec<ModelDescriptor>) -> ProviderRegistration {
    ProviderRegistration::builder(provider)
        .models(models)
        .api(ApiId::new(API), Arc::new(NoopApi))
        .build()
        .unwrap()
}

fn local_dynamic_provider(
    provider: &str,
    source: Rc<dyn LocalModelCatalogSource>,
) -> LocalProviderRegistration {
    LocalProviderRegistration::builder(provider)
        .auth(Rc::new(AnonymousAuthResolver))
        .catalog_source(source)
        .api(ApiId::new(API), Rc::new(LocalNoopApi))
        .build()
        .unwrap()
}

fn local_static_provider(
    provider: &str,
    models: Vec<ModelDescriptor>,
) -> LocalProviderRegistration {
    LocalProviderRegistration::builder(provider)
        .models(models)
        .api(ApiId::new(API), Rc::new(LocalNoopApi))
        .build()
        .unwrap()
}

fn selected(provider: &str) -> RefreshRequest {
    RefreshRequest {
        providers: Some(BTreeSet::from([ProviderId::new(provider)])),
        ..RefreshRequest::default()
    }
}

fn model_name(models: &Models, provider: &str, id: &str) -> Option<String> {
    models
        .model(&ModelRef::new(provider, id))
        .map(|model| model.common.display_name)
}

fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
    let (mutex, condition) = &**gate;
    *lock(mutex) = true;
    condition.notify_all();
}

#[test]
fn catalog_reads_last_published_snapshot_synchronously() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:250-760";
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let source = Arc::new(BlockingSource {
        baseline: Arc::from(vec![test_model(PROVIDER, "old", "old")]),
        fresh: candidate(vec![test_model(PROVIDER, "new", "new")], "new"),
        started: started_tx,
        release: Arc::clone(&gate),
    });
    let models = Models::builder()
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    let worker_models = models.clone();
    let worker = thread::spawn(move || {
        block_on(worker_models.refresh(selected(PROVIDER), CancellationToken::new()))
    });
    started_rx.recv().unwrap();
    assert!(models.model(&ModelRef::new(PROVIDER, "old")).is_some());
    assert!(models.model(&ModelRef::new(PROVIDER, "new")).is_none());
    release(&gate);
    worker.join().unwrap();
    assert!(models.model(&ModelRef::new(PROVIDER, "new")).is_some());

    // An explicitly supplied catalog is itself the latest-snapshot owner. The
    // builder must preserve it rather than capturing one baseline snapshot and
    // replacing it with an unrelated managed catalog.
    let custom_catalog = Arc::new(MutableModelCatalog::new(vec![test_model(
        PROVIDER,
        "custom-old",
        "custom-old",
    )]));
    let custom_models = Models::builder()
        .provider(
            ProviderRegistration::builder(PROVIDER)
                .catalog(custom_catalog.clone())
                .api(ApiId::new(API), Arc::new(NoopApi))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    custom_catalog.publish(vec![test_model(PROVIDER, "custom-new", "custom-new")]);
    assert!(
        custom_models
            .model(&ModelRef::new(PROVIDER, "custom-old"))
            .is_none()
    );
    assert!(
        custom_models
            .model(&ModelRef::new(PROVIDER, "custom-new"))
            .is_some()
    );

    let local_catalog = Rc::new(LocalMutableModelCatalog::new(vec![test_model(
        PROVIDER,
        "local-custom-old",
        "local-custom-old",
    )]));
    let local_models = LocalModels::builder()
        .provider(
            LocalProviderRegistration::builder(PROVIDER)
                .catalog(local_catalog.clone())
                .api(ApiId::new(API), Rc::new(LocalNoopApi))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    local_catalog.publish(vec![test_model(
        PROVIDER,
        "local-custom-new",
        "local-custom-new",
    )]);
    assert!(
        local_models
            .model(&ModelRef::new(PROVIDER, "local-custom-old"))
            .is_none()
    );
    assert!(
        local_models
            .model(&ModelRef::new(PROVIDER, "local-custom-new"))
            .is_some()
    );
}

#[test]
fn catalog_refresh_candidate_with_unregistered_api_rejects_publication() {
    // §10.7 catalog publish contract; Pi basis:
    // packages/ai/src/models.ts createProvider API dispatch and refresh
    // transaction, as tightened by architecture v2 part 2 §5.4–§5.5.
    let mut invalid = test_model(PROVIDER, "invalid", "Invalid");
    invalid.api = ApiModelConfig::Custom(CustomApiModelConfig {
        api: ApiId::new("unregistered-api"),
        schema_version: 1,
        value: RawValue::from_string("{}".into()).unwrap(),
    });
    let source = Arc::new(QueueSource::new(
        vec![test_model(PROVIDER, "baseline", "Baseline")],
        vec![Ok(candidate(vec![invalid], "invalid"))],
    ));
    let store = Arc::new(InMemoryModelsStore::default());
    let models = Models::builder()
        .models_store(store.clone())
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();

    let report = block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(matches!(
        report.providers.get(&ProviderId::new(PROVIDER)),
        Some(ProviderRefreshResult::Failed { .. })
    ));
    assert_eq!(
        model_name(&models, PROVIDER, "baseline").as_deref(),
        Some("Baseline")
    );
    assert!(
        block_on(ModelsStore::read(
            store.as_ref(),
            &ProviderId::new(PROVIDER),
            CancellationToken::new(),
        ))
        .unwrap()
        .is_none()
    );
}

#[test]
fn catalog_static_refresh_is_noop() {
    let _basis =
        "architecture v2 part 1 §3.7; part 2 §5.7, §10.7; packages/ai/src/models.ts:306-361";
    let models = Models::builder()
        .provider(static_provider(
            PROVIDER,
            vec![test_model(PROVIDER, "static", "static")],
        ))
        .build()
        .unwrap();
    let report = block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(report.providers.is_empty());
    assert_eq!(
        model_name(&models, PROVIDER, "static").as_deref(),
        Some("static")
    );

    let local = LocalModels::builder()
        .provider(local_static_provider(
            PROVIDER,
            vec![test_model(PROVIDER, "local-static", "local-static")],
        ))
        .build()
        .unwrap();
    let local_report = block_on(local.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(local_report.providers.is_empty());
    assert!(
        local
            .model(&ModelRef::new(PROVIDER, "local-static"))
            .is_some()
    );
}

#[test]
fn catalog_restore_precedes_auth_resolution() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:357-466";
    let store = Arc::new(InMemoryModelsStore::default());
    block_on(ModelsStore::write(
        store.as_ref(),
        &ProviderId::new(PROVIDER),
        &persisted(vec![test_model(PROVIDER, "cached", "cached")], "cached"),
        CancellationToken::new(),
    ))
    .unwrap();
    let slot = Arc::new(Mutex::new(None::<Models>));
    let auth_slot = Arc::clone(&slot);
    let auth = Arc::new(ProbeAuth {
        probe: Arc::new(move || {
            assert!(
                lock(&auth_slot)
                    .as_ref()
                    .unwrap()
                    .model(&ModelRef::new(PROVIDER, "cached"))
                    .is_some()
            );
        }),
    });
    let source = Arc::new(QueueSource::new(
        Vec::new(),
        vec![Ok(candidate(
            vec![test_model(PROVIDER, "fresh", "fresh")],
            "fresh",
        ))],
    ));
    let models = Models::builder()
        .models_store(store)
        .provider(dynamic_provider(PROVIDER, source, auth))
        .build()
        .unwrap();
    *lock(&slot) = Some(models.clone());
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));

    // Pinned Pi's catalog path uses actual expiry, unlike request auth's
    // five-minute minimum-validity window. A token with one minute remaining
    // must reach catalog fetch unchanged in both Send and local families.
    let credentials = Arc::new(InMemoryCredentialStore::new());
    block_on(async {
        let mut lease = credentials
            .acquire_lease(ProviderId::new(PROVIDER), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(catalog_oauth_credential("still-valid", 61_000)));
        lease.commit().await.unwrap();
    });
    let refreshes = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(
        QueueSource::new(
            Vec::new(),
            vec![Ok(candidate(
                vec![test_model(PROVIDER, "send-fresh", "send-fresh")],
                "send-fresh",
            ))],
        )
        .inspect(Arc::new(|context| {
            assert_eq!(
                context
                    .auth
                    .api_key
                    .as_ref()
                    .expect("catalog OAuth access token")
                    .expose_secret(),
                "still-valid"
            );
        })),
    );
    let resolver = ProviderAuthResolver::new(
        None,
        Some(Arc::new(CatalogOAuth {
            refreshes: Arc::clone(&refreshes),
        })),
    )
    .with_clock(Arc::new(FixedCatalogAuthClock(
        Timestamp::from_unix_millis(1_000),
    )));
    let send_models = Models::builder()
        .credential_store(credentials)
        .provider(dynamic_provider(PROVIDER, source, Arc::new(resolver)))
        .build()
        .unwrap();
    block_on(send_models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    assert!(
        send_models
            .model(&ModelRef::new(PROVIDER, "send-fresh"))
            .is_some()
    );

    let local_credentials = Rc::new(LocalInMemoryCredentialStore::new());
    block_on(async {
        let mut lease = LocalCredentialStore::acquire_lease(
            local_credentials.as_ref(),
            ProviderId::new(PROVIDER),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        lease.replace(Some(catalog_oauth_credential("still-valid-local", 61_000)));
        lease.commit().await.unwrap();
    });
    let local_refreshes = Rc::new(Cell::new(0));
    let local_resolver = LocalProviderAuthResolver::new(
        None,
        Some(Rc::new(LocalCatalogOAuth {
            refreshes: Rc::clone(&local_refreshes),
        })),
    )
    .with_clock(Rc::new(FixedCatalogAuthClock(Timestamp::from_unix_millis(
        1_000,
    ))));
    let local_models = LocalModels::builder()
        .credential_store(Rc::clone(&local_credentials) as Rc<dyn LocalCredentialStore>)
        .provider(
            LocalProviderRegistration::builder(PROVIDER)
                .auth(Rc::new(local_resolver))
                .catalog_source(Rc::new(LocalQueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(PROVIDER, "local-fresh", "local-fresh")],
                        "local-fresh",
                    ))],
                )))
                .api(ApiId::new(API), Rc::new(LocalNoopApi))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    block_on(local_models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert_eq!(local_refreshes.get(), 0);
    assert!(
        local_models
            .model(&ModelRef::new(PROVIDER, "local-fresh"))
            .is_some()
    );
}

#[test]
fn catalog_restore_precedes_network() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:357-384";
    let store = Arc::new(InMemoryModelsStore::default());
    block_on(ModelsStore::write(
        store.as_ref(),
        &ProviderId::new(PROVIDER),
        &persisted(vec![test_model(PROVIDER, "cached", "cached")], "cached"),
        CancellationToken::new(),
    ))
    .unwrap();
    let slot = Arc::new(Mutex::new(None::<Models>));
    let fetch_slot = Arc::clone(&slot);
    let source = Arc::new(
        QueueSource::new(
            Vec::new(),
            vec![Ok(candidate(
                vec![test_model(PROVIDER, "fresh", "fresh")],
                "fresh",
            ))],
        )
        .inspect(Arc::new(move |context| {
            assert_eq!(
                context.stored.as_ref().unwrap().revision.as_deref(),
                Some("cached")
            );
            assert!(
                fetch_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .unwrap()
                    .model(&ModelRef::new(PROVIDER, "cached"))
                    .is_some()
            );
        })),
    );
    let models = Models::builder()
        .models_store(store)
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    *lock(&slot) = Some(models.clone());
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
}

#[test]
fn catalog_network_refresh_is_best_effort_per_provider() {
    let _basis = "architecture v2 part 2 §5.5, §5.7, §10.7; packages/ai/src/models.ts:306-420";
    let good = "good-provider";
    let bad = "bad-provider";
    let models = Models::builder()
        .provider(dynamic_provider(
            good,
            Arc::new(QueueSource::new(
                Vec::new(),
                vec![Ok(candidate(
                    vec![test_model(good, "fresh", "fresh")],
                    "fresh",
                ))],
            )),
            Arc::new(AnonymousAuthResolver),
        ))
        .provider(dynamic_provider(
            bad,
            Arc::new(QueueSource::new(
                Vec::new(),
                vec![Err(CatalogError::source("fetch failed"))],
            )),
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    let report = block_on(models.refresh(RefreshRequest::default(), CancellationToken::new()));
    assert!(matches!(
        report.providers.get(&ProviderId::new(good)),
        Some(ProviderRefreshResult::Refreshed { .. })
    ));
    assert!(matches!(
        report.providers.get(&ProviderId::new(bad)),
        Some(ProviderRefreshResult::Failed { .. })
    ));
    assert!(models.model(&ModelRef::new(good, "fresh")).is_some());
}

#[test]
fn catalog_superseded_refresh_cannot_publish() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:259-364,386-390";

    // Once a durable write has been submitted, cancellation must not drop its
    // non-cancellation-safe future or release provider publication sequencing.
    {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (second_started_tx, _second_started_rx) = std::sync::mpsc::channel();
        let release_first = Arc::new((Mutex::new(false), Condvar::new()));
        let store = Arc::new(BackgroundFirstWriteStore {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            calls: AtomicUsize::new(0),
            first_started: started_tx,
            second_started: second_started_tx,
            release_first: Arc::clone(&release_first),
        });
        let models = Models::builder()
            .models_store(store.clone())
            .provider(dynamic_provider(
                PROVIDER,
                Arc::new(QueueSource::new(
                    vec![test_model(PROVIDER, "baseline", "baseline")],
                    vec![Ok(candidate(
                        vec![test_model(PROVIDER, "cancelled", "cancelled")],
                        "cancelled",
                    ))],
                )),
                Arc::new(AnonymousAuthResolver),
            ))
            .build()
            .unwrap();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker_models = models.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(block_on(
                    worker_models.refresh(selected(PROVIDER), worker_cancellation),
                ))
                .unwrap();
        });
        started_rx.recv().unwrap();
        cancellation.cancel();
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err());
        release(&release_first);
        let report = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("durable completion must release the cancelled refresh");
        worker.join().unwrap();
        assert!(report.aborted);
        assert!(report.providers.is_empty());
        assert!(models.model(&ModelRef::new(PROVIDER, "baseline")).is_some());
        assert!(
            models
                .model(&ModelRef::new(PROVIDER, "cancelled"))
                .is_none()
        );
        let durable = block_on(ModelsStore::read(
            store.as_ref(),
            &ProviderId::new(PROVIDER),
            CancellationToken::new(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(durable.revision.as_deref(), Some("cancelled"));
    }

    // An already-cancelled Send caller returns before provider selection and
    // therefore cannot supersede a refresh that was already publishing.
    {
        let (first_started_tx, first_started_rx) = std::sync::mpsc::channel();
        let (second_started_tx, _second_started_rx) = std::sync::mpsc::channel();
        let release_first = Arc::new((Mutex::new(false), Condvar::new()));
        let store = Arc::new(BlockingFirstWriteStore {
            inner: InMemoryModelsStore::default(),
            calls: AtomicUsize::new(0),
            first_started: first_started_tx,
            second_started: second_started_tx,
            release_first: Arc::clone(&release_first),
        });
        let models = Models::builder()
            .models_store(store)
            .provider(dynamic_provider(
                PROVIDER,
                Arc::new(QueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(PROVIDER, "send-active", "send-active")],
                        "send-active",
                    ))],
                )),
                Arc::new(AnonymousAuthResolver),
            ))
            .build()
            .unwrap();
        let worker_models = models.clone();
        let worker = thread::spawn(move || {
            block_on(worker_models.refresh(selected(PROVIDER), CancellationToken::new()))
        });
        first_started_rx.recv().unwrap();

        let already_cancelled = CancellationToken::new();
        already_cancelled.cancel();
        let cancelled_report = block_on(models.refresh(selected(PROVIDER), already_cancelled));
        let returned_before_selection =
            cancelled_report.aborted && cancelled_report.providers.is_empty();

        release(&release_first);
        let active_report = worker.join().unwrap();
        assert!(returned_before_selection);
        assert!(matches!(
            active_report.providers.get(&ProviderId::new(PROVIDER)),
            Some(ProviderRefreshResult::Refreshed { .. })
        ));
        assert!(
            models
                .model(&ModelRef::new(PROVIDER, "send-active"))
                .is_some()
        );
    }

    // The Local family has the same pre-cancelled entry contract. Manually
    // polling the first future leaves it inside its first durable write.
    {
        let release_first = Rc::new(Cell::new(false));
        let store = Rc::new(LocalBlockingFirstWriteStore {
            entries: RefCell::new(BTreeMap::new()),
            calls: Cell::new(0),
            release_first: Rc::clone(&release_first),
        });
        let models = LocalModels::builder()
            .models_store(store.clone())
            .provider(local_dynamic_provider(
                PROVIDER,
                Rc::new(LocalQueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(PROVIDER, "local-active", "local-active")],
                        "local-active",
                    ))],
                )),
            ))
            .build()
            .unwrap();
        let mut active = Box::pin(models.refresh(selected(PROVIDER), CancellationToken::new()));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(active.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(store.calls.get(), 1);

        let already_cancelled = CancellationToken::new();
        already_cancelled.cancel();
        let cancelled_report = block_on(models.refresh(selected(PROVIDER), already_cancelled));
        release_first.set(true);
        let active_report = block_on(active);

        assert!(cancelled_report.aborted);
        assert!(cancelled_report.providers.is_empty());
        assert!(matches!(
            active_report.providers.get(&ProviderId::new(PROVIDER)),
            Some(ProviderRefreshResult::Refreshed { .. })
        ));
        assert!(
            models
                .model(&ModelRef::new(PROVIDER, "local-active"))
                .is_some()
        );
    }

    // A replacement can fetch concurrently, but its durable write must remain
    // behind background I/O that survives dropping the old write future. This
    // proves stale durable data cannot commit last after supersession.
    {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let release_first = Arc::new((Mutex::new(false), Condvar::new()));
        let store = Arc::new(BackgroundFirstWriteStore {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            calls: AtomicUsize::new(0),
            first_started: started_tx,
            second_started: second_started_tx,
            release_first: Arc::clone(&release_first),
        });
        let old = dynamic_provider(
            PROVIDER,
            Arc::new(QueueSource::new(
                Vec::new(),
                vec![Ok(candidate(
                    vec![test_model(PROVIDER, "old-generation", "old")],
                    "old-generation",
                ))],
            )),
            Arc::new(AnonymousAuthResolver),
        );
        let models = Models::builder()
            .models_store(store.clone())
            .provider(old)
            .build()
            .unwrap();
        let worker_models = models.clone();
        let old_worker = thread::spawn(move || {
            block_on(worker_models.refresh(selected(PROVIDER), CancellationToken::new()))
        });
        started_rx.recv().unwrap();

        let replacement = dynamic_provider(
            PROVIDER,
            Arc::new(QueueSource::new(
                Vec::new(),
                vec![Ok(candidate(
                    vec![test_model(PROVIDER, "new-generation", "new")],
                    "new-generation",
                ))],
            )),
            Arc::new(AnonymousAuthResolver),
        );
        models.set_provider(replacement).unwrap();
        let replacement_models = models.clone();
        let new_worker = thread::spawn(move || {
            block_on(replacement_models.refresh(selected(PROVIDER), CancellationToken::new()))
        });
        assert!(
            second_started_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "replacement durable write started before prior background I/O completed"
        );
        release(&release_first);
        let old_report = old_worker.join().unwrap();
        let report = new_worker.join().unwrap();
        assert!(old_report.providers.is_empty());
        assert!(matches!(
            report.providers.get(&ProviderId::new(PROVIDER)),
            Some(ProviderRefreshResult::Refreshed { .. })
        ));
        assert!(
            models
                .model(&ModelRef::new(PROVIDER, "new-generation"))
                .is_some()
        );
        assert!(
            models
                .model(&ModelRef::new(PROVIDER, "old-generation"))
                .is_none()
        );
        let stored = block_on(ModelsStore::read(
            store.as_ref(),
            &ProviderId::new(PROVIDER),
            CancellationToken::new(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(stored.revision.as_deref(), Some("new-generation"));
    }

    // Delete/re-add retains Send coordination by provider ID. The replacement
    // can fetch concurrently, but its durable publication must remain behind
    // the non-cooperative old write so that fresh state is durable last.
    {
        let (first_started_tx, first_started_rx) = std::sync::mpsc::channel();
        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let (replacement_fetched_tx, replacement_fetched_rx) = std::sync::mpsc::channel();
        let release_first = Arc::new((Mutex::new(false), Condvar::new()));
        let store = Arc::new(BlockingFirstWriteStore {
            inner: InMemoryModelsStore::default(),
            calls: AtomicUsize::new(0),
            first_started: first_started_tx,
            second_started: second_started_tx,
            release_first: Arc::clone(&release_first),
        });
        let models = Models::builder()
            .models_store(store.clone())
            .provider(dynamic_provider(
                PROVIDER,
                Arc::new(QueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(PROVIDER, "deleted-old", "deleted-old")],
                        "deleted-old",
                    ))],
                )),
                Arc::new(AnonymousAuthResolver),
            ))
            .build()
            .unwrap();
        let old_models = models.clone();
        let old_worker = thread::spawn(move || {
            block_on(old_models.refresh(selected(PROVIDER), CancellationToken::new()))
        });
        first_started_rx.recv().unwrap();

        assert!(models.remove_provider(&ProviderId::new(PROVIDER)).is_some());
        let replacement_source = QueueSource::new(
            Vec::new(),
            vec![Ok(candidate(
                vec![test_model(PROVIDER, "readded-new", "readded-new")],
                "readded-new",
            ))],
        )
        .inspect(Arc::new(move |_| replacement_fetched_tx.send(()).unwrap()));
        models
            .set_provider(dynamic_provider(
                PROVIDER,
                Arc::new(replacement_source),
                Arc::new(AnonymousAuthResolver),
            ))
            .unwrap();
        let new_models = models.clone();
        let new_worker = thread::spawn(move || {
            block_on(new_models.refresh(selected(PROVIDER), CancellationToken::new()))
        });
        replacement_fetched_rx.recv().unwrap();
        let publication_was_serialized = second_started_rx
            .recv_timeout(Duration::from_millis(200))
            .is_err();

        release(&release_first);
        let old_report = old_worker.join().unwrap();
        let new_report = new_worker.join().unwrap();
        assert!(publication_was_serialized);
        assert!(old_report.providers.is_empty());
        assert!(matches!(
            new_report.providers.get(&ProviderId::new(PROVIDER)),
            Some(ProviderRefreshResult::Refreshed { .. })
        ));
        let stored = block_on(ModelsStore::read(
            store.as_ref(),
            &ProviderId::new(PROVIDER),
            CancellationToken::new(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(stored.revision.as_deref(), Some("readded-new"));
    }

    // Local delete/re-add retains the same publication mutex. Both refresh
    // futures are manually driven so the write ordering is deterministic.
    {
        let release_first = Rc::new(Cell::new(false));
        let store = Rc::new(LocalBlockingFirstWriteStore {
            entries: RefCell::new(BTreeMap::new()),
            calls: Cell::new(0),
            release_first: Rc::clone(&release_first),
        });
        let models = LocalModels::builder()
            .models_store(store.clone())
            .provider(local_dynamic_provider(
                PROVIDER,
                Rc::new(LocalQueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(
                            PROVIDER,
                            "local-deleted-old",
                            "local-deleted-old",
                        )],
                        "local-deleted-old",
                    ))],
                )),
            ))
            .build()
            .unwrap();
        let mut old_refresh =
            Box::pin(models.refresh(selected(PROVIDER), CancellationToken::new()));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            old_refresh.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(store.calls.get(), 1);

        assert!(models.remove_provider(&ProviderId::new(PROVIDER)).is_some());
        models
            .set_provider(local_dynamic_provider(
                PROVIDER,
                Rc::new(LocalQueueSource::new(
                    Vec::new(),
                    vec![Ok(candidate(
                        vec![test_model(
                            PROVIDER,
                            "local-readded-new",
                            "local-readded-new",
                        )],
                        "local-readded-new",
                    ))],
                )),
            ))
            .unwrap();
        let mut new_refresh =
            Box::pin(models.refresh(selected(PROVIDER), CancellationToken::new()));
        let early_new_report = match new_refresh.as_mut().poll(&mut context) {
            Poll::Ready(report) => Some(report),
            Poll::Pending => None,
        };
        let publication_was_serialized = store.calls.get() == 1;

        release_first.set(true);
        let old_report = block_on(old_refresh);
        let new_report = early_new_report.unwrap_or_else(|| block_on(new_refresh));
        assert!(publication_was_serialized);
        assert!(old_report.providers.is_empty());
        assert!(matches!(
            new_report.providers.get(&ProviderId::new(PROVIDER)),
            Some(ProviderRefreshResult::Refreshed { .. })
        ));
        let stored = block_on(LocalModelsStore::read(
            store.as_ref(),
            &ProviderId::new(PROVIDER),
            CancellationToken::new(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(stored.revision.as_deref(), Some("local-readded-new"));
    }
}

struct ObservingStore {
    inner: InMemoryModelsStore,
    models: Mutex<Option<Models>>,
    observed_old: AtomicBool,
}

impl ModelsStore for ObservingStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        ModelsStore::read(&self.inner, provider, cancellation)
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        assert!(
            lock(&self.models)
                .as_ref()
                .unwrap()
                .model(&ModelRef::new(PROVIDER, "fresh"))
                .is_none()
        );
        self.observed_old.store(true, Ordering::SeqCst);
        ModelsStore::write(&self.inner, provider, snapshot, cancellation)
    }

    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        ModelsStore::delete(&self.inner, provider, cancellation)
    }
}

#[test]
fn catalog_persist_precedes_publish() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:306-357";
    let store = Arc::new(ObservingStore {
        inner: InMemoryModelsStore::default(),
        models: Mutex::new(None),
        observed_old: AtomicBool::new(false),
    });
    let source = Arc::new(QueueSource::new(
        vec![test_model(PROVIDER, "old", "old")],
        vec![Ok(candidate(
            vec![test_model(PROVIDER, "fresh", "fresh")],
            "fresh",
        ))],
    ));
    let models = Models::builder()
        .models_store(store.clone())
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    *lock(&store.models) = Some(models.clone());
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(store.observed_old.load(Ordering::SeqCst));
    assert!(models.model(&ModelRef::new(PROVIDER, "fresh")).is_some());
}

struct FailingWriteStore;

impl ModelsStore for FailingWriteStore {
    fn read(
        &self,
        _provider: &ProviderId,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn write(
        &self,
        _provider: &ProviderId,
        _snapshot: &PersistedCatalogSnapshot,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async { Err(StoreError::new("write_failed", "durable write failed")) })
    }

    fn delete(
        &self,
        _provider: &ProviderId,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn catalog_failed_persist_keeps_old_snapshot() {
    let _basis = "architecture v2 part 2 §5.5, §10.7; packages/ai/src/models.ts:306-357";
    let source = Arc::new(QueueSource::new(
        vec![test_model(PROVIDER, "old", "old")],
        vec![Ok(candidate(
            vec![test_model(PROVIDER, "fresh", "fresh")],
            "fresh",
        ))],
    ));
    let models = Models::builder()
        .models_store(Arc::new(FailingWriteStore))
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    let report = block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(matches!(
        report.providers.get(&ProviderId::new(PROVIDER)),
        Some(ProviderRefreshResult::Failed { .. })
    ));
    assert!(models.model(&ModelRef::new(PROVIDER, "old")).is_some());
    assert!(models.model(&ModelRef::new(PROVIDER, "fresh")).is_none());
}

#[test]
fn catalog_reader_never_sees_partial_candidate() {
    let _basis = "architecture v2 part 1 §3.7; part 2 §5.5, §10.7";
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let source = Arc::new(BlockingSource {
        baseline: Arc::from(vec![test_model(PROVIDER, "old", "old")]),
        fresh: candidate(
            vec![
                test_model(PROVIDER, "fresh-a", "fresh-a"),
                test_model(PROVIDER, "fresh-b", "fresh-b"),
            ],
            "fresh",
        ),
        started: started_tx,
        release: Arc::clone(&gate),
    });
    let models = Models::builder()
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    let worker_models = models.clone();
    let worker = thread::spawn(move || {
        block_on(worker_models.refresh(selected(PROVIDER), CancellationToken::new()))
    });
    started_rx.recv().unwrap();
    for _ in 0..100 {
        let snapshot = models.catalog_snapshot(&ProviderId::new(PROVIDER)).unwrap();
        let ids = snapshot
            .models
            .iter()
            .map(|model| model.common.model_ref.model.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["old"]);
    }
    release(&gate);
    worker.join().unwrap();
    let snapshot = models.catalog_snapshot(&ProviderId::new(PROVIDER)).unwrap();
    assert_eq!(snapshot.models.len(), 3);
    assert_eq!(snapshot.revision.as_deref(), Some("fresh"));
}

fn display_patch(value: &str) -> ModelOverridePatch {
    ModelOverridePatch {
        display_name: Some(value.into()),
        ..ModelOverridePatch::default()
    }
}

fn overridden_models(
    store: Arc<InMemoryModelsStore>,
    overrides: Arc<InMemoryModelOverrideStore>,
    network_name: &str,
) -> Models {
    let source = Arc::new(QueueSource::new(
        Vec::new(),
        vec![Ok(candidate(
            vec![test_model(PROVIDER, "dynamic", network_name)],
            "dynamic",
        ))],
    ));
    Models::builder()
        .models_store(store)
        .model_override_store(overrides)
        .provider(dynamic_provider(
            PROVIDER,
            source,
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap()
}

struct RacingOverrideStore {
    calls: AtomicUsize,
    current: Mutex<Vec<ModelOverride>>,
    stale_publication_snapshot: Vec<ModelOverride>,
    fresh_snapshot: Vec<ModelOverride>,
    models: Mutex<Option<Models>>,
}

impl ModelOverrideStore for RacingOverrideStore {
    fn snapshot(&self, provider: &ProviderId) -> Result<Arc<[ModelOverride]>, OverrideError> {
        assert_eq!(provider, &ProviderId::new(PROVIDER));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 1 {
            *lock(&self.current) = self.fresh_snapshot.clone();
            let models = lock(&self.models).as_ref().unwrap().clone();
            let result = models
                .refresh_host_overrides()
                .remove(&ProviderId::new(PROVIDER))
                .unwrap();
            assert!(result.is_ok());
            return Ok(Arc::from(self.stale_publication_snapshot.clone()));
        }
        Ok(Arc::from(lock(&self.current).clone()))
    }
}

#[test]
fn catalog_host_override_applies_after_dynamic_snapshot() {
    let _basis = "architecture v2 part 2 §5.3, §5.5, §10.7";
    let stale = vec![ModelOverride::patch(
        ModelRef::new(PROVIDER, "dynamic"),
        display_patch("stale-host"),
    )];
    let fresh = vec![ModelOverride::patch(
        ModelRef::new(PROVIDER, "dynamic"),
        display_patch("fresh-host"),
    )];
    let overrides = Arc::new(RacingOverrideStore {
        calls: AtomicUsize::new(0),
        current: Mutex::new(stale.clone()),
        stale_publication_snapshot: stale,
        fresh_snapshot: fresh,
        models: Mutex::new(None),
    });
    let models = Models::builder()
        .models_store(Arc::new(InMemoryModelsStore::default()))
        .model_override_store(overrides.clone())
        .provider(dynamic_provider(
            PROVIDER,
            Arc::new(QueueSource::new(
                Vec::new(),
                vec![Ok(candidate(
                    vec![test_model(PROVIDER, "dynamic", "provider")],
                    "dynamic",
                ))],
            )),
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    *lock(&overrides.models) = Some(models.clone());
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert_eq!(
        model_name(&models, PROVIDER, "dynamic").as_deref(),
        Some("fresh-host")
    );
}

#[test]
fn catalog_removed_override_reveals_provider_value() {
    let _basis = "architecture v2 part 2 §5.6, §10.7";
    let overrides = Arc::new(InMemoryModelOverrideStore::default());
    overrides
        .replace(
            ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "dynamic"),
                display_patch("host"),
            )],
        )
        .unwrap();
    let models = overridden_models(
        Arc::new(InMemoryModelsStore::default()),
        overrides.clone(),
        "provider",
    );
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    assert_eq!(
        model_name(&models, PROVIDER, "dynamic").as_deref(),
        Some("host")
    );
    overrides.remove(&ProviderId::new(PROVIDER));
    assert!(
        models
            .refresh_host_overrides()
            .remove(&ProviderId::new(PROVIDER))
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        model_name(&models, PROVIDER, "dynamic").as_deref(),
        Some("provider")
    );
}

#[test]
fn catalog_raw_snapshot_does_not_contain_flattened_override() {
    let _basis = "architecture v2 part 2 §5.3, §5.6, §10.7";
    let store = Arc::new(InMemoryModelsStore::default());
    let overrides = Arc::new(InMemoryModelOverrideStore::default());
    overrides
        .replace(
            ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "dynamic"),
                display_patch("host"),
            )],
        )
        .unwrap();
    let models = overridden_models(store.clone(), overrides, "provider");
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    let raw = block_on(ModelsStore::read(
        store.as_ref(),
        &ProviderId::new(PROVIDER),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(raw.models[0].common.display_name, "provider");
    assert_eq!(
        model_name(&models, PROVIDER, "dynamic").as_deref(),
        Some("host")
    );
}

#[test]
fn catalog_typed_compat_mismatch_is_rejected() {
    let _basis = "architecture v2 part 2 §5.3, §10.7";
    let models = Models::builder()
        .provider(static_provider(
            PROVIDER,
            vec![test_model(PROVIDER, "typed", "typed")],
        ))
        .build()
        .unwrap();
    let mismatch = ModelOverridePatch {
        api: Some(ApiId::new("different-api")),
        ..ModelOverridePatch::default()
    };
    let error = models
        .set_runtime_overrides(
            &ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "typed"),
                mismatch,
            )],
        )
        .unwrap_err();
    assert_eq!(error.code, "catalog_validation");
    assert_eq!(
        model_name(&models, PROVIDER, "typed").as_deref(),
        Some("typed")
    );
}

#[test]
fn catalog_unknown_extensions_round_trip() {
    let _basis = "architecture v2 part 2 §5.1, §5.4, §10.7";
    let store = Arc::new(InMemoryModelsStore::default());
    let mut extended = test_model(PROVIDER, "extended", "extended");
    extended.extensions.insert(
        ExtensionId::new("vendor.model"),
        VersionedExtension {
            schema_version: 7,
            value: RawValue::from_string("{\"opaque\":[1,2,3]}".into()).unwrap(),
        },
    );
    let mut fresh = candidate(vec![extended.clone()], "extended");
    fresh.source_metadata.insert(
        ExtensionId::new("vendor.catalog"),
        VersionedExtension {
            schema_version: 3,
            value: RawValue::from_string("{\"cursor\":\"next\"}".into()).unwrap(),
        },
    );
    let models = Models::builder()
        .models_store(store.clone())
        .provider(dynamic_provider(
            PROVIDER,
            Arc::new(QueueSource::new(Vec::new(), vec![Ok(fresh)])),
            Arc::new(AnonymousAuthResolver),
        ))
        .build()
        .unwrap();
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    let raw = block_on(ModelsStore::read(
        store.as_ref(),
        &ProviderId::new(PROVIDER),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    let bytes = serde_json::to_vec(&raw).unwrap();
    let restored: PersistedCatalogSnapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, raw);
    assert_eq!(restored.models[0].extensions, extended.extensions);
    assert_eq!(restored.source_metadata, raw.source_metadata);
}

#[test]
fn catalog_runtime_override_has_highest_precedence() {
    let _basis = "architecture v2 part 2 §5.3, §10.7";
    let overrides = Arc::new(InMemoryModelOverrideStore::default());
    overrides
        .replace(
            ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "dynamic"),
                display_patch("host"),
            )],
        )
        .unwrap();
    let models = overridden_models(
        Arc::new(InMemoryModelsStore::default()),
        overrides,
        "provider",
    );
    block_on(models.refresh(selected(PROVIDER), CancellationToken::new()));
    models
        .set_runtime_overrides(
            &ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "dynamic"),
                display_patch("runtime"),
            )],
        )
        .unwrap();
    assert_eq!(
        model_name(&models, PROVIDER, "dynamic").as_deref(),
        Some("runtime")
    );

    // Part 2 §9.2 requires the same catalog control plane for the non-Send
    // family, including local source/store/override wiring and persistence.
    let local_store = Rc::new(LocalInMemoryModelsStore::default());
    let local_overrides = Rc::new(LocalInMemoryModelOverrideStore::default());
    local_overrides
        .replace(
            ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "local-dynamic"),
                display_patch("local-host"),
            )],
        )
        .unwrap();
    let local = LocalModels::builder()
        .models_store(local_store.clone())
        .model_override_store(local_overrides)
        .provider(local_dynamic_provider(
            PROVIDER,
            Rc::new(LocalQueueSource::new(
                Vec::new(),
                vec![Ok(candidate(
                    vec![test_model(PROVIDER, "local-dynamic", "local-provider")],
                    "local-dynamic",
                ))],
            )),
        ))
        .build()
        .unwrap();
    let local_report = block_on(local.refresh(selected(PROVIDER), CancellationToken::new()));
    assert!(matches!(
        local_report.providers.get(&ProviderId::new(PROVIDER)),
        Some(ProviderRefreshResult::Refreshed { .. })
    ));
    assert_eq!(
        local
            .model(&ModelRef::new(PROVIDER, "local-dynamic"))
            .unwrap()
            .common
            .display_name,
        "local-host"
    );
    let local_raw = block_on(LocalModelsStore::read(
        local_store.as_ref(),
        &ProviderId::new(PROVIDER),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(local_raw.models[0].common.display_name, "local-provider");
    local
        .set_runtime_overrides(
            &ProviderId::new(PROVIDER),
            vec![ModelOverride::patch(
                ModelRef::new(PROVIDER, "local-dynamic"),
                display_patch("local-runtime"),
            )],
        )
        .unwrap();
    assert_eq!(
        local
            .model(&ModelRef::new(PROVIDER, "local-dynamic"))
            .unwrap()
            .common
            .display_name,
        "local-runtime"
    );
    assert_eq!(
        local
            .catalog_snapshot(&ProviderId::new(PROVIDER))
            .unwrap()
            .revision
            .as_deref(),
        Some("local-dynamic")
    );
}
