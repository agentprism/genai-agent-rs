use agentprism_ai::*;
use futures_executor::block_on;
use futures_util::future::join;
use serde_json::value::RawValue;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use url::Url;

const AUTH_BASIS: &str = "architecture v2 part 1 §3.8; architecture v2 part 2 §6.1-§6.4, §6.6, §9.2, §10.7; packages/ai/src/models.ts:621-696; packages/ai/src/auth/types.ts:1-240; packages/ai/src/auth/credential-store.ts:1-67; packages/ai/src/auth/resolve.ts:1-205; packages/ai/src/auth/oauth/openai-codex.ts:120-520";
const DEVICE_BASIS: &str = "architecture v2 part 2 §6.1, §10.7; packages/ai/src/auth/oauth/device-code.ts:1-98; packages/ai/test/oauth-device-code.test.ts:1-145";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn api_credential(value: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential {
        key: Some(SecretString::new(value)),
        environment: BTreeMap::new(),
    })
}

fn oauth_credential(access: &str, expires_at: i64) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new("refresh-secret"),
        expires_at: Timestamp::from_unix_millis(expires_at),
        extra: ProviderOAuthExtra::None,
    })
}

fn put(store: &InMemoryCredentialStore, provider: &str, credential: Credential) {
    block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new(provider), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(credential));
        lease.commit().await.unwrap();
    });
}

fn put_local(store: &LocalInMemoryCredentialStore, provider: &str, credential: Credential) {
    block_on(async {
        let mut lease = LocalCredentialStore::acquire_lease(
            store,
            ProviderId::new(provider),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        lease.replace(Some(credential));
        lease.commit().await.unwrap();
    });
}

fn resolved_key(auth: &ResolvedAuth) -> &str {
    auth.api_key
        .as_ref()
        .expect("resolved API key")
        .expose_secret()
}

#[derive(Clone)]
struct FixedClock(Timestamp);

impl AuthClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

impl LocalAuthClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

struct FakeOAuth {
    refreshes: Arc<AtomicUsize>,
    fail_refresh: bool,
    refreshed_expiry: Timestamp,
}

impl OAuthAuth for FakeOAuth {
    fn name(&self) -> &str {
        "Fake OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        let expiry = self.refreshed_expiry;
        Box::pin(async move {
            Ok(OAuthCredential {
                access: SecretString::new("login-access"),
                refresh: SecretString::new("login-refresh"),
                expires_at: expiry,
                extra: ProviderOAuthExtra::None,
            })
        })
    }

    fn refresh(
        &self,
        _credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        let refresh_number = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        let fail = self.fail_refresh;
        let expiry = self.refreshed_expiry;
        Box::pin(async move {
            if fail {
                return Err(AuthError::new("invalid_grant", "invalid_grant"));
            }
            Ok(OAuthCredential {
                access: SecretString::new(format!("refreshed-{refresh_number}")),
                refresh: SecretString::new("rotated-refresh"),
                expires_at: expiry,
                extra: ProviderOAuthExtra::None,
            })
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move {
            Ok(ResolvedAuth {
                api_key: Some(access),
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

struct FakeLocalOAuth {
    refreshes: Rc<Cell<usize>>,
    refreshed_expiry: Timestamp,
}

impl LocalOAuthAuth for FakeLocalOAuth {
    fn name(&self) -> &str {
        "Fake local OAuth"
    }

    fn login(
        &self,
        _interaction: Rc<dyn LocalAuthInteraction>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        let expiry = self.refreshed_expiry;
        Box::pin(async move {
            Ok(OAuthCredential {
                access: SecretString::new("local-login-access"),
                refresh: SecretString::new("local-login-refresh"),
                expires_at: expiry,
                extra: ProviderOAuthExtra::None,
            })
        })
    }

    fn refresh(
        &self,
        _credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        let refresh_number = self.refreshes.get() + 1;
        self.refreshes.set(refresh_number);
        let expiry = self.refreshed_expiry;
        Box::pin(async move {
            Ok(OAuthCredential {
                access: SecretString::new(format!("local-refreshed-{refresh_number}")),
                refresh: SecretString::new("local-rotated-refresh"),
                expires_at: expiry,
                extra: ProviderOAuthExtra::None,
            })
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let access = credential.access.clone();
        Box::pin(async move {
            Ok(ResolvedAuth {
                api_key: Some(access),
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

fn models_with_auth(
    store: Arc<InMemoryCredentialStore>,
    context: Arc<dyn AuthContext>,
    resolver: ProviderAuthResolver,
) -> Models {
    let provider = ProviderRegistration::builder("auth-provider")
        .auth(Arc::new(resolver))
        .build()
        .unwrap();
    Models::builder()
        .credential_store(store)
        .auth_context(context)
        .provider(provider)
        .build()
        .unwrap()
}

fn standard_resolver(oauth: Option<Arc<dyn OAuthAuth>>) -> ProviderAuthResolver {
    ProviderAuthResolver::new(
        Some(Arc::new(EnvironmentApiKeyAuth::new(
            "fixture API key",
            ["FIXTURE_API_KEY"],
        ))),
        oauth,
    )
    .with_clock(Arc::new(FixedClock(Timestamp::from_unix_millis(1_000))))
}

fn local_standard_resolver() -> LocalProviderAuthResolver {
    LocalProviderAuthResolver::new(
        Some(Rc::new(EnvironmentApiKeyAuth::new(
            "fixture API key",
            ["FIXTURE_API_KEY"],
        ))),
        None,
    )
    .with_clock(Rc::new(FixedClock(Timestamp::from_unix_millis(1_000))))
}

fn local_models_with_auth(
    store: Rc<LocalInMemoryCredentialStore>,
    context: Rc<dyn LocalAuthContext>,
    resolver: LocalProviderAuthResolver,
) -> LocalModels {
    let provider = LocalProviderRegistration::builder("auth-provider")
        .auth(Rc::new(resolver))
        .build()
        .unwrap();
    LocalModels::builder()
        .credential_store(store)
        .auth_context(context)
        .provider(provider)
        .build()
        .unwrap()
}

const LOCAL_AUTH_API: &str = "local-auth-test-api";

fn local_auth_model() -> ModelDescriptor {
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("local-auth-provider", "local-auth-model"),
            display_name: "local auth model".into(),
            base_url: Url::parse("https://auth.invalid/v1").unwrap(),
            modalities: ModalityCapabilities::default(),
            limits: ModelLimits {
                context_window: 8_192,
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
            api: ApiId::new(LOCAL_AUTH_API),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

#[derive(Clone)]
struct CapturingLocalAuth {
    seen: Rc<RefCell<Vec<(AuthResolutionPurpose, AuthResolutionOverrides)>>>,
}

impl LocalAuthResolver for CapturingLocalAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let key = request.overrides.api_key.clone();
        self.seen
            .borrow_mut()
            .push((request.purpose, request.overrides));
        Box::pin(async move {
            Ok(Some(ResolvedAuth {
                api_key: key,
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("local explicit override"),
            }))
        })
    }
}

struct RecordingLocalApi {
    keys: Rc<RefCell<Vec<Option<String>>>>,
}

impl LocalChatApi for RecordingLocalApi {
    fn stream(
        &self,
        request: LocalResolvedApiRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        self.keys.borrow_mut().push(
            request
                .api_key
                .as_ref()
                .map(|key| key.expose_secret().to_owned()),
        );
        Box::pin(async { Ok(LocalAssistantStream::new(futures_util::stream::empty())) })
    }
}

#[derive(Default)]
struct FakeInteraction {
    capabilities: AuthHostCapabilities,
    answers: Mutex<VecDeque<Result<AuthAnswer, AuthInteractionError>>>,
    prompts: Mutex<Vec<AuthPrompt>>,
    notifications: Mutex<Vec<AuthEvent>>,
    receiver: Mutex<Option<Box<dyn RedirectReceiver>>>,
    receiver_requests: AtomicUsize,
}

impl FakeInteraction {
    fn with_answers(answers: impl IntoIterator<Item = AuthAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().map(Ok).collect()),
            ..Self::default()
        }
    }
}

impl AuthInteraction for FakeInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        self.capabilities.clone()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        lock(&self.prompts).push(prompt);
        let answer = lock(&self.answers).pop_front().unwrap_or_else(|| {
            Err(AuthInteractionError::Failed {
                code: "missing_answer".into(),
                message: "fake interaction has no answer".into(),
            })
        });
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(AuthInteractionError::Cancelled)
            } else {
                answer
            }
        })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        lock(&self.notifications).push(event);
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        self.receiver_requests.fetch_add(1, Ordering::SeqCst);
        let receiver = lock(&self.receiver).take();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AuthInteractionError::Cancelled);
            }
            receiver.ok_or_else(|| AuthInteractionError::Unsupported {
                message: "fake host has no redirect receiver".into(),
            })
        })
    }
}

struct ImmediateReceiver {
    redirect_uri: Url,
    arrival: Url,
}

impl RedirectReceiver for ImmediateReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AuthInteractionError::Cancelled);
            }
            Ok(RedirectArrival {
                url: self.arrival,
                received_at: Timestamp::from_unix_millis(10),
            })
        })
    }
}

#[test]
fn auth_explicit_request_value_wins() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    put(store.as_ref(), "auth-provider", api_credential("stored"));
    let context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
        [],
    ));
    let models = models_with_auth(store, context, standard_resolver(None));
    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("explicit")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "explicit");

    // Part 2 §9.2 requires the local Models execution surface to carry the
    // same non-serializable auth overrides as the Send surface.
    let seen = Rc::new(RefCell::new(Vec::new()));
    let keys = Rc::new(RefCell::new(Vec::new()));
    let local = LocalModels::builder()
        .provider(
            LocalProviderRegistration::builder("local-auth-provider")
                .auth(Rc::new(CapturingLocalAuth {
                    seen: Rc::clone(&seen),
                }))
                .models(vec![local_auth_model()])
                .api(
                    ApiId::new(LOCAL_AUTH_API),
                    Rc::new(RecordingLocalApi {
                        keys: Rc::clone(&keys),
                    }),
                )
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let local_overrides = AuthResolutionOverrides {
        api_key: Some(SecretString::new("local-explicit")),
        environment: BTreeMap::from([("ACCOUNT_ID".into(), "local-account".into())]),
        min_oauth_validity: Some(Duration::from_secs(30 * 60)),
    };
    block_on(local.stream_simple_with_auth(
        ModelRequest {
            model: ModelRef::new("local-auth-provider", "local-auth-model"),
            context: agentprism_ai::Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        local_overrides,
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(keys.borrow().as_slice(), &[Some("local-explicit".into())]);
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, AuthResolutionPurpose::Request);
    assert_eq!(
        seen[0].1.api_key.as_ref().unwrap().expose_secret(),
        "local-explicit"
    );
    assert_eq!(seen[0].1.environment["ACCOUNT_ID"], "local-account");
    assert_eq!(
        seen[0].1.min_oauth_validity,
        Some(Duration::from_secs(30 * 60))
    );
}

#[test]
fn auth_stored_credential_owns_provider() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    put(store.as_ref(), "auth-provider", api_credential("stored"));
    let context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
        [],
    ));
    let models = models_with_auth(store, context, standard_resolver(None));
    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "stored");
}

#[test]
fn auth_environment_used_only_without_stored_credential() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    let context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
        [],
    ));
    let models = models_with_auth(store.clone(), context, standard_resolver(None));
    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "ambient");
    assert_eq!(resolved.source, AuthSource::new("FIXTURE_API_KEY"));

    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            environment: BTreeMap::from([("FIXTURE_API_KEY".into(), String::new())]),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "ambient");

    // envApiKeyAuth uses JavaScript truthiness, not trimming: an empty stored
    // key falls through, while whitespace remains a configured credential.
    put(store.as_ref(), "auth-provider", api_credential(""));
    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "ambient");

    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            environment: BTreeMap::from([("FIXTURE_API_KEY".into(), "   ".into())]),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "   ");

    put(store.as_ref(), "auth-provider", api_credential("stored"));
    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "ambient");

    let resolved = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("   ")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "   ");

    // The local resolver family follows the same truthiness and overlay
    // rules, including an empty explicit key bypassing a stored nonempty key.
    let local_store = Rc::new(LocalInMemoryCredentialStore::new());
    let local_context: Rc<dyn LocalAuthContext> = Rc::new(MapAuthContext::new(
        BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
        [],
    ));
    let local = local_models_with_auth(
        Rc::clone(&local_store),
        local_context,
        local_standard_resolver(),
    );
    put_local(local_store.as_ref(), "auth-provider", api_credential(""));
    let resolved = block_on(local.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            environment: BTreeMap::from([("FIXTURE_API_KEY".into(), "   ".into())]),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "   ");

    put_local(
        local_store.as_ref(),
        "auth-provider",
        api_credential("stored"),
    );
    let resolved = block_on(local.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            api_key: Some(SecretString::new("")),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(resolved_key(&resolved), "ambient");
}

#[test]
fn auth_failed_oauth_refresh_never_falls_back_to_env_in_memory() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    put(
        store.as_ref(),
        "auth-provider",
        oauth_credential("expired", 0),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth = Arc::new(FakeOAuth {
        refreshes: Arc::clone(&refreshes),
        fail_refresh: true,
        refreshed_expiry: Timestamp::from_unix_millis(3_601_000),
    });
    let context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
        [],
    ));
    let models = models_with_auth(store.clone(), context, standard_resolver(Some(oauth)));
    let error = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .unwrap_err();
    assert_eq!(error.code(), "oauth");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let stored = block_on(store.read(ProviderId::new("auth-provider"), CancellationToken::new()))
        .unwrap()
        .unwrap();
    let Credential::OAuth(stored) = stored else {
        panic!("OAuth credential retained");
    };
    assert_eq!(stored.access.expose_secret(), "expired");
}

#[test]
fn auth_oauth_refresh_is_serialized_in_memory() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    put(
        store.as_ref(),
        "auth-provider",
        oauth_credential("expired", 0),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth = Arc::new(FakeOAuth {
        refreshes: Arc::clone(&refreshes),
        fail_refresh: false,
        refreshed_expiry: Timestamp::from_unix_millis(3_601_000),
    });
    let models = models_with_auth(
        store,
        Arc::new(EmptyAuthContext),
        standard_resolver(Some(oauth)),
    );
    let (left, right) = block_on(join(
        models.resolve_auth(
            ProviderId::new("auth-provider"),
            AuthResolutionOverrides::default(),
            CancellationToken::new(),
        ),
        models.resolve_auth(
            ProviderId::new("auth-provider"),
            AuthResolutionOverrides::default(),
            CancellationToken::new(),
        ),
    ));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(resolved_key(left.unwrap().as_ref().unwrap()), "refreshed-1");
    assert_eq!(
        resolved_key(right.unwrap().as_ref().unwrap()),
        "refreshed-1"
    );
}

#[test]
fn auth_oauth_rotation_persists_before_explicit_minimum_rejection() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    put(
        store.as_ref(),
        "auth-provider",
        oauth_credential("expired", 0),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth = Arc::new(FakeOAuth {
        refreshes: Arc::clone(&refreshes),
        fail_refresh: false,
        refreshed_expiry: Timestamp::from_unix_millis(1_801_000),
    });
    let models = models_with_auth(
        store.clone(),
        Arc::new(EmptyAuthContext),
        standard_resolver(Some(oauth)),
    );

    let error = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            min_oauth_validity: Some(Duration::from_secs(60 * 60)),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap_err();

    assert_eq!(error.code(), "oauth");
    assert!(error.to_string().contains("expires too soon"));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let stored = block_on(store.read(ProviderId::new("auth-provider"), CancellationToken::new()))
        .unwrap()
        .unwrap();
    let Credential::OAuth(stored) = stored else {
        panic!("rotated OAuth credential was persisted");
    };
    assert_eq!(stored.access.expose_secret(), "refreshed-1");
    assert_eq!(stored.refresh.expose_secret(), "rotated-refresh");
}

#[test]
fn auth_local_oauth_rotation_persists_before_explicit_minimum_rejection() {
    let _basis = AUTH_BASIS;
    let store = Rc::new(LocalInMemoryCredentialStore::new());
    put_local(
        store.as_ref(),
        "auth-provider",
        oauth_credential("local-expired", 0),
    );
    let refreshes = Rc::new(Cell::new(0));
    let resolver = LocalProviderAuthResolver::new(
        None,
        Some(Rc::new(FakeLocalOAuth {
            refreshes: Rc::clone(&refreshes),
            refreshed_expiry: Timestamp::from_unix_millis(1_801_000),
        })),
    )
    .with_clock(Rc::new(FixedClock(Timestamp::from_unix_millis(1_000))));
    let models = local_models_with_auth(Rc::clone(&store), Rc::new(EmptyAuthContext), resolver);

    let error = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides {
            min_oauth_validity: Some(Duration::from_secs(60 * 60)),
            ..AuthResolutionOverrides::default()
        },
        CancellationToken::new(),
    ))
    .unwrap_err();

    assert_eq!(error.code(), "oauth");
    assert!(error.to_string().contains("expires too soon"));
    assert_eq!(refreshes.get(), 1);
    let stored = block_on(LocalCredentialStore::read(
        store.as_ref(),
        ProviderId::new("auth-provider"),
        CancellationToken::new(),
    ))
    .unwrap()
    .unwrap();
    let Credential::OAuth(stored) = stored else {
        panic!("rotated local OAuth credential was persisted");
    };
    assert_eq!(stored.access.expose_secret(), "local-refreshed-1");
    assert_eq!(stored.refresh.expose_secret(), "local-rotated-refresh");
}

#[test]
fn auth_login_persists_under_modify_in_memory() {
    let _basis = AUTH_BASIS;
    let store = Arc::new(InMemoryCredentialStore::new());
    let models = models_with_auth(
        store.clone(),
        Arc::new(EmptyAuthContext),
        standard_resolver(None),
    );
    let interaction = Arc::new(FakeInteraction::with_answers([AuthAnswer::Text(
        "login-secret".into(),
    )]));
    let credential = block_on(models.login(
        ProviderId::new("auth-provider"),
        interaction,
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(credential, api_credential("login-secret"));
    assert_eq!(
        block_on(store.read(ProviderId::new("auth-provider"), CancellationToken::new(),)).unwrap(),
        Some(credential)
    );
}

#[test]
fn auth_list_never_resolves_secrets() {
    let _basis = AUTH_BASIS;
    let store = InMemoryCredentialStore::new();
    put(&store, "api", api_credential("list-secret-api"));
    put(&store, "oauth", oauth_credential("list-secret-oauth", 10));
    let info = block_on(store.list(CancellationToken::new())).unwrap();
    assert_eq!(
        info,
        vec![
            CredentialInfo {
                provider: ProviderId::new("api"),
                credential_type: CredentialType::ApiKey,
            },
            CredentialInfo {
                provider: ProviderId::new("oauth"),
                credential_type: CredentialType::OAuth,
            },
        ]
    );
    let debug = format!("{info:?} {store:?}");
    assert!(!debug.contains("list-secret-api"));
    assert!(!debug.contains("list-secret-oauth"));
}

#[test]
fn auth_text_prompt() {
    let _basis = AUTH_BASIS;
    let interaction = FakeInteraction::with_answers([AuthAnswer::Text("typed".into())]);
    let answer = block_on(interaction.prompt(
        AuthPrompt::Text {
            message: "Name".into(),
            placeholder: Some("Ada".into()),
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(answer, AuthAnswer::Text("typed".into()));
    assert!(matches!(
        lock(&interaction.prompts)[0],
        AuthPrompt::Text { .. }
    ));
}

#[test]
fn auth_secret_prompt() {
    let _basis = AUTH_BASIS;
    let interaction = FakeInteraction::with_answers([AuthAnswer::Text("secret".into())]);
    let answer = block_on(interaction.prompt(
        AuthPrompt::Secret {
            message: "Token".into(),
            placeholder: None,
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(answer, AuthAnswer::Text("secret".into()));
    assert!(matches!(
        lock(&interaction.prompts)[0],
        AuthPrompt::Secret { .. }
    ));
}

#[test]
fn auth_select_returns_option_id() {
    let _basis = AUTH_BASIS;
    let interaction = FakeInteraction::with_answers([AuthAnswer::Selected("device".into())]);
    let answer = block_on(interaction.prompt(
        AuthPrompt::Select {
            message: "Method".into(),
            options: vec![AuthSelectOption {
                id: "device".into(),
                label: "Device code".into(),
                description: None,
            }],
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(answer, AuthAnswer::Selected("device".into()));
}

#[test]
fn auth_manual_code_can_be_cancelled_by_callback() {
    let _basis = AUTH_BASIS;
    let manual_token = Arc::new(Mutex::new(None::<CancellationToken>));
    let captured = Arc::clone(&manual_token);
    let receiver = Box::new(ImmediateReceiver {
        redirect_uri: Url::parse("http://127.0.0.1:1455/auth/callback").unwrap(),
        arrival: Url::parse("http://127.0.0.1:1455/auth/callback?code=callback").unwrap(),
    });
    let winner = block_on(select_first_valid(
        move |cancellation| async move {
            let arrival = receiver.receive(cancellation).await?;
            Ok(arrival.url.to_string())
        },
        move |cancellation| {
            *lock(&captured) = Some(cancellation.clone());
            async move {
                cancellation.cancelled().await;
                Err(AuthError::Cancelled)
            }
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert!(winner.contains("code=callback"));
    assert!(lock(&manual_token).as_ref().unwrap().is_cancelled());
}

struct FakeDeviceRuntime {
    now: AtomicI64,
    sleeps: Mutex<Vec<Duration>>,
    cancel_during_sleep: bool,
}

impl FakeDeviceRuntime {
    fn new(cancel_during_sleep: bool) -> Self {
        Self {
            now: AtomicI64::new(0),
            sleeps: Mutex::new(Vec::new()),
            cancel_during_sleep,
        }
    }
}

impl OAuthDeviceCodeRuntime for FakeDeviceRuntime {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.now.load(Ordering::SeqCst))
    }

    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AuthError>> {
        lock(&self.sleeps).push(duration);
        self.now.fetch_add(
            i64::try_from(duration.as_millis()).unwrap(),
            Ordering::SeqCst,
        );
        let cancel = self.cancel_during_sleep;
        Box::pin(async move {
            if cancel {
                cancellation.cancel();
            }
            cancellation.check().map_err(|_| AuthError::Cancelled)
        })
    }
}

struct SequencePoll {
    outcomes: VecDeque<OAuthDeviceCodePollResult<String>>,
}

impl SequencePoll {
    fn new(outcomes: impl IntoIterator<Item = OAuthDeviceCodePollResult<String>>) -> Self {
        Self {
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl OAuthDeviceCodePoll<String> for SequencePoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<String>, AuthError>> {
        let result = self.outcomes.pop_front().expect("one fake poll outcome");
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(result)
        })
    }
}

fn poll_device(
    runtime: Arc<FakeDeviceRuntime>,
    interval: Option<Duration>,
    expires_in: Option<Duration>,
    outcomes: impl IntoIterator<Item = OAuthDeviceCodePollResult<String>>,
) -> Result<String, AuthError> {
    block_on(poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
        interval,
        expires_in,
        wait_before_first_poll: false,
        poll: Box::new(SequencePoll::new(outcomes)),
        cancellation: CancellationToken::new(),
        runtime,
    }))
}

#[test]
fn auth_device_default_interval_is_five_seconds() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(false));
    let value = poll_device(
        runtime.clone(),
        None,
        Some(Duration::from_secs(30)),
        [
            OAuthDeviceCodePollResult::Pending,
            OAuthDeviceCodePollResult::Complete("token".into()),
        ],
    )
    .unwrap();
    assert_eq!(value, "token");
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(5)]);
}

#[test]
fn auth_device_interval_minimum_is_one_second() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(false));
    poll_device(
        runtime.clone(),
        Some(Duration::from_millis(250)),
        Some(Duration::from_secs(30)),
        [
            OAuthDeviceCodePollResult::Pending,
            OAuthDeviceCodePollResult::Complete("token".into()),
        ],
    )
    .unwrap();
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(1)]);
}

#[test]
fn auth_device_slow_down_adds_five_seconds() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(false));
    poll_device(
        runtime.clone(),
        Some(Duration::from_secs(2)),
        Some(Duration::from_secs(30)),
        [
            OAuthDeviceCodePollResult::SlowDown { interval: None },
            OAuthDeviceCodePollResult::Complete("token".into()),
        ],
    )
    .unwrap();
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(7)]);
}

#[test]
fn auth_device_server_interval_wins() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(false));
    poll_device(
        runtime.clone(),
        Some(Duration::from_secs(2)),
        Some(Duration::from_secs(60)),
        [
            OAuthDeviceCodePollResult::SlowDown {
                interval: Some(Duration::from_secs(30)),
            },
            OAuthDeviceCodePollResult::Complete("token".into()),
        ],
    )
    .unwrap();
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(30)]);
}

#[test]
fn auth_device_deadline_is_enforced() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(false));
    let error = poll_device(
        runtime.clone(),
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(3)),
        [OAuthDeviceCodePollResult::Pending],
    )
    .unwrap_err();
    assert_eq!(error.code(), "device_code_timeout");
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(3)]);
}

#[test]
fn auth_device_poll_is_cancellable() {
    let _basis = DEVICE_BASIS;
    let runtime = Arc::new(FakeDeviceRuntime::new(true));
    let error = poll_device(
        runtime.clone(),
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(30)),
        [OAuthDeviceCodePollResult::Pending],
    )
    .unwrap_err();
    assert_eq!(error, AuthError::Cancelled);
    assert_eq!(*lock(&runtime.sleeps), vec![Duration::from_secs(5)]);
}

#[test]
fn auth_pkce_state_is_validated() {
    let _basis = AUTH_BASIS;
    let pkce = pkce_from_random_bytes([0; 32]);
    assert_eq!(pkce.verifier, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(pkce.verifier.len(), 43);
    assert_eq!(pkce.challenge.len(), 43);
    let state = oauth_state_from_random_bytes([0xab; 16]);
    assert_eq!(state, "abababababababababababababababab");
    validate_oauth_state(&state, &state).unwrap();
    assert_eq!(
        validate_oauth_state(&state, "wrong"),
        Err(AuthError::StateMismatch)
    );
    let parsed = parse_oauth_authorization_input(
        "myapp://oauth/callback?code=authorization-code&state=abab",
    );
    assert_eq!(parsed.code.as_deref(), Some("authorization-code"));
    assert_eq!(parsed.state.as_deref(), Some("abab"));

    let parsed = parse_oauth_authorization_input("?code=query-code&state=query-state");
    assert_eq!(parsed.code.as_deref(), Some("query-code"));
    assert_eq!(parsed.state.as_deref(), Some("query-state"));

    let parsed = parse_oauth_authorization_input("fragment-code#fragment-state#ignored");
    assert_eq!(parsed.code.as_deref(), Some("fragment-code"));
    assert_eq!(parsed.state.as_deref(), Some("fragment-state"));
}

#[test]
fn auth_callback_and_manual_first_valid_wins() {
    let _basis = AUTH_BASIS;
    let winner = block_on(select_first_valid(
        |_cancellation| async { Err::<String, _>(AuthError::StateMismatch) },
        |_cancellation| async { Ok::<String, AuthError>("manual-code".into()) },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(winner, "manual-code");
}

struct FakeClosedChallenge {
    challenge_id: AuthChallengeId,
    cancellation: CancellationToken,
}

impl FakeClosedChallenge {
    fn respond(&self) -> Result<(), AuthInteractionError> {
        if self.cancellation.is_cancelled() {
            Err(AuthInteractionError::ChallengeSuperseded {
                challenge_id: self.challenge_id.clone(),
            })
        } else {
            Ok(())
        }
    }
}

#[test]
fn auth_late_losing_response_is_superseded() {
    let _basis = AUTH_BASIS;
    let losing_token = Arc::new(Mutex::new(None::<CancellationToken>));
    let captured = Arc::clone(&losing_token);
    let winner = block_on(select_first_valid(
        |_cancellation| async { Ok::<String, AuthError>("callback".into()) },
        move |cancellation| {
            *lock(&captured) = Some(cancellation.clone());
            async move {
                cancellation.cancelled().await;
                Err(AuthError::Cancelled)
            }
        },
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(winner, "callback");
    let challenge = FakeClosedChallenge {
        challenge_id: AuthChallengeId::new("challenge-7"),
        cancellation: lock(&losing_token).as_ref().unwrap().clone(),
    };
    assert_eq!(
        challenge.respond(),
        Err(AuthInteractionError::ChallengeSuperseded {
            challenge_id: AuthChallengeId::new("challenge-7"),
        })
    );
}

fn redirect_request(strategy: RedirectStrategy) -> RedirectReceiverRequest {
    RedirectReceiverRequest {
        challenge_id: AuthChallengeId::new("mobile-challenge"),
        preferred: vec![strategy],
        expected_path: Some("/oauth/callback".into()),
        success_page: AuthHtmlPage {
            html: "success".into(),
        },
        failure_page: AuthHtmlPage {
            html: "failure".into(),
        },
    }
}

#[test]
fn auth_mobile_custom_scheme_flow() {
    let _basis = AUTH_BASIS;
    let mut interaction = FakeInteraction {
        capabilities: AuthHostCapabilities {
            custom_url_scheme: true,
            manual_paste: true,
            ..AuthHostCapabilities::default()
        },
        ..FakeInteraction::default()
    };
    *interaction.receiver.get_mut().unwrap() = Some(Box::new(ImmediateReceiver {
        redirect_uri: Url::parse("myapp://oauth/callback").unwrap(),
        arrival: Url::parse("myapp://oauth/callback?code=mobile&state=state").unwrap(),
    }));
    let interaction = Arc::new(interaction);
    let receiver = block_on(create_supported_redirect_receiver(
        ProviderId::new("mobile-provider"),
        interaction.clone(),
        redirect_request(RedirectStrategy::CustomScheme {
            scheme: "myapp".into(),
            path: "/oauth/callback".into(),
        }),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(receiver.redirect_uri().as_str(), "myapp://oauth/callback");
    let arrival = block_on(receiver.receive(CancellationToken::new())).unwrap();
    assert_eq!(arrival.url.query_pairs().next().unwrap().1, "mobile");
    assert_eq!(interaction.receiver_requests.load(Ordering::SeqCst), 1);
}

#[test]
fn auth_mobile_unsupported_fixed_loopback_is_explicit() {
    let _basis = AUTH_BASIS;
    let interaction = Arc::new(FakeInteraction {
        capabilities: AuthHostCapabilities {
            custom_url_scheme: true,
            ..AuthHostCapabilities::default()
        },
        ..FakeInteraction::default()
    });
    let error = match block_on(create_supported_redirect_receiver(
        ProviderId::new("fixed-loopback-provider"),
        interaction.clone(),
        redirect_request(RedirectStrategy::FixedLoopback {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 1455,
            path: "/oauth/callback".into(),
        }),
        CancellationToken::new(),
    )) {
        Ok(_) => panic!("fixed loopback must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        AuthError::UnsupportedRedirectStrategy {
            provider,
            host_capabilities,
            ..
        } if provider == ProviderId::new("fixed-loopback-provider")
            && !host_capabilities.loopback_http
    ));
    assert_eq!(interaction.receiver_requests.load(Ordering::SeqCst), 0);
}

#[test]
fn auth_provider_extra_fields_round_trip() {
    let _basis = AUTH_BASIS;
    let values = vec![
        ProviderOAuthExtra::None,
        ProviderOAuthExtra::Radius {
            gateway_url: Url::parse("https://radius.example/gateway").unwrap(),
            organization_id: Some("org-1".into()),
        },
        ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: Url::parse("https://copilot.example/api").unwrap(),
            account_id: Some("account-1".into()),
            enterprise_url: Some("enterprise.example".into()),
            available_model_ids: Some(vec![ModelId::new("entitled-model")]),
        },
        ProviderOAuthExtra::OpenAiCodex {
            account_id: "account-2".into(),
        },
        ProviderOAuthExtra::Custom {
            schema: ExtensionId::new("example.auth"),
            schema_version: 7,
            value: RawValue::from_string(r#"{"tenant":"one"}"#.into()).unwrap(),
        },
    ];
    for value in values {
        let bytes = serde_json::to_vec(&value).unwrap();
        let restored: ProviderOAuthExtra = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, value);
    }
}
