#![cfg(not(target_arch = "wasm32"))]

use agentprism_ai::*;
use futures_executor::block_on;
use futures_util::future::join;
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FILE_AUTH_BASIS: &str = "architecture v2 part 1 §3.8; architecture v2 part 2 §6.6, §10.7; packages/ai/src/auth/credential-store.ts:1-67; packages/ai/src/auth/resolve.ts:1-205; packages/ai/src/models.ts:580-640; packages/ai/test/models-runtime.test.ts:840-963";

fn oauth_credential(access: &str, refresh: &str, expires_at: i64) -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires_at: Timestamp::from_unix_millis(expires_at),
        extra: ProviderOAuthExtra::None,
    })
}

fn put(store: &dyn CredentialStore, provider: &str, credential: Credential) {
    block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new(provider), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(credential));
        lease.commit().await.unwrap();
    });
}

fn models_with_auth(
    store: Arc<dyn CredentialStore>,
    context: Arc<dyn AuthContext>,
    resolver: ProviderAuthResolver,
) -> Models {
    Models::builder()
        .credential_store(store)
        .auth_context(context)
        .provider(
            ProviderRegistration::builder("auth-provider")
                .auth(Arc::new(resolver))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap()
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

struct FileOAuth {
    refreshes: Arc<AtomicUsize>,
    fail_refresh: bool,
    delay: Duration,
    refreshed_expiry: Timestamp,
}

impl OAuthAuth for FileOAuth {
    fn name(&self) -> &str {
        "File OAuth"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "login is not used by this fixture".into(),
            })
        })
    }

    fn refresh(
        &self,
        _credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        let refresh_number = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        let fail_refresh = self.fail_refresh;
        let delay = self.delay;
        let expires_at = self.refreshed_expiry;
        Box::pin(async move {
            if !delay.is_zero() {
                futures_timer::Delay::new(delay).await;
            }
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            if fail_refresh {
                return Err(AuthError::new("invalid_grant", "invalid_grant"));
            }
            Ok(OAuthCredential {
                access: SecretString::new(format!("refreshed-{refresh_number}")),
                refresh: SecretString::new("rotated-refresh"),
                expires_at,
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
                environment: Default::default(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

fn resolver(oauth: Arc<dyn OAuthAuth>) -> ProviderAuthResolver {
    ProviderAuthResolver::new(
        Some(Arc::new(EnvironmentApiKeyAuth::new(
            "fixture API key",
            ["FIXTURE_API_KEY"],
        ))),
        Some(oauth),
    )
    .with_clock(Arc::new(FixedClock(Timestamp::from_unix_millis(1_000))))
}

struct LoginApiKeyAuth;

impl ApiKeyAuth for LoginApiKeyAuth {
    fn name(&self) -> &str {
        "File API key"
    }

    fn resolve(
        &self,
        _request: ApiKeyResolveRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(None) })
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async {
            Ok(ApiKeyCredential {
                key: Some(SecretString::new("login-secret")),
                environment: BTreeMap::from([("ACCOUNT_ID".into(), "account-1".into())]),
            })
        })
    }
}

struct NoopInteraction;

impl AuthInteraction for NoopInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        Box::pin(async {
            Err(AuthInteractionError::Unsupported {
                message: "prompt is not used by this fixture".into(),
            })
        })
    }

    fn notify(&self, _event: AuthEvent) -> Result<(), AuthInteractionError> {
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async {
            Err(AuthInteractionError::Unsupported {
                message: "redirects are not used by this fixture".into(),
            })
        })
    }
}

#[test]
fn auth_oauth_refresh_is_serialized() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let first_store = Arc::new(FileCredentialStore::new(path.clone()));
    let second_store = Arc::new(FileCredentialStore::new(path.clone()));
    put(
        first_store.as_ref(),
        "auth-provider",
        oauth_credential("expired", "refresh-secret", 0),
    );

    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth: Arc<dyn OAuthAuth> = Arc::new(FileOAuth {
        refreshes: Arc::clone(&refreshes),
        fail_refresh: false,
        delay: Duration::from_millis(25),
        refreshed_expiry: Timestamp::from_unix_millis(3_601_000),
    });
    let first = models_with_auth(
        first_store,
        Arc::new(EmptyAuthContext),
        resolver(Arc::clone(&oauth)),
    );
    let second = models_with_auth(second_store, Arc::new(EmptyAuthContext), resolver(oauth));

    let (left, right) = block_on(join(
        first.resolve_auth(
            ProviderId::new("auth-provider"),
            AuthResolutionOverrides::default(),
            CancellationToken::new(),
        ),
        second.resolve_auth(
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
    let reopened = FileCredentialStore::new(path);
    let Credential::OAuth(stored) =
        block_on(reopened.read(ProviderId::new("auth-provider"), CancellationToken::new()))
            .unwrap()
            .unwrap()
    else {
        panic!("refreshed OAuth credential was persisted");
    };
    assert_eq!(stored.access.expose_secret(), "refreshed-1");
    assert_eq!(stored.refresh.expose_secret(), "rotated-refresh");
}

#[test]
fn auth_failed_oauth_refresh_never_falls_back_to_env() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = Arc::new(FileCredentialStore::new(path.clone()));
    put(
        store.as_ref(),
        "auth-provider",
        oauth_credential("expired", "refresh-secret", 0),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let oauth: Arc<dyn OAuthAuth> = Arc::new(FileOAuth {
        refreshes: Arc::clone(&refreshes),
        fail_refresh: true,
        delay: Duration::ZERO,
        refreshed_expiry: Timestamp::from_unix_millis(3_601_000),
    });
    let models = models_with_auth(
        store,
        Arc::new(MapAuthContext::new(
            BTreeMap::from([("FIXTURE_API_KEY".into(), "ambient".into())]),
            [],
        )),
        resolver(oauth),
    );

    let error = block_on(models.resolve_auth(
        ProviderId::new("auth-provider"),
        AuthResolutionOverrides::default(),
        CancellationToken::new(),
    ))
    .unwrap_err();

    assert_eq!(error.code(), "oauth");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let reopened = FileCredentialStore::new(path);
    let Credential::OAuth(stored) =
        block_on(reopened.read(ProviderId::new("auth-provider"), CancellationToken::new()))
            .unwrap()
            .unwrap()
    else {
        panic!("failed refresh retained the OAuth credential");
    };
    assert_eq!(stored.access.expose_secret(), "expired");
    assert_eq!(stored.refresh.expose_secret(), "refresh-secret");
}

#[test]
fn auth_login_persists_under_modify() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = Arc::new(FileCredentialStore::new(path.clone()));
    let resolver = ProviderAuthResolver::new(Some(Arc::new(LoginApiKeyAuth)), None);
    let models = models_with_auth(store, Arc::new(EmptyAuthContext), resolver);

    let credential = block_on(models.login(
        ProviderId::new("auth-provider"),
        Arc::new(NoopInteraction),
        CancellationToken::new(),
    ))
    .unwrap();

    let reopened = FileCredentialStore::new(path.clone());
    assert_eq!(
        block_on(reopened.read(ProviderId::new("auth-provider"), CancellationToken::new(),))
            .unwrap(),
        Some(credential)
    );
    let document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        document["schema_version"],
        Value::from(CREDENTIAL_FILE_SCHEMA_VERSION)
    );
    assert_eq!(document["credentials"]["auth-provider"]["type"], "api_key");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(reopened.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn auth_file_store_round_trips_provider_extras() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = FileCredentialStore::new(path.clone());
    let credential = Credential::OAuth(OAuthCredential {
        access: SecretString::new("access-secret"),
        refresh: SecretString::new("refresh-secret"),
        expires_at: Timestamp::from_unix_millis(42),
        extra: ProviderOAuthExtra::Custom {
            schema: ExtensionId::new("example.auth"),
            schema_version: 7,
            value: RawValue::from_string(r#"{"tenant":"one"}"#.into()).unwrap(),
        },
    });
    put(&store, "custom-provider", credential.clone());

    let reopened = FileCredentialStore::new(path);
    assert_eq!(
        block_on(reopened.read(ProviderId::new("custom-provider"), CancellationToken::new(),))
            .unwrap(),
        Some(credential)
    );
}

#[test]
fn auth_file_store_round_trips_github_copilot_enterprise_metadata() {
    // Architecture v2 part 2 §6.6 and §10.7; Pi basis:
    // packages/ai/src/auth/oauth/github-copilot.ts:341-347,487-505.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = FileCredentialStore::new(path.clone());
    let credential = Credential::OAuth(OAuthCredential {
        access: SecretString::new("copilot-access"),
        refresh: SecretString::new("github-refresh"),
        expires_at: Timestamp::from_unix_millis(42),
        extra: ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: url::Url::parse("https://copilot-api.enterprise.example").unwrap(),
            account_id: Some("account-identity".into()),
            enterprise_url: Some("enterprise.example".into()),
            available_model_ids: Some(vec![ModelId::new("entitled-model")]),
        },
    });
    put(&store, "github-copilot", credential.clone());

    let document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        document["credentials"]["github-copilot"]["extra"]["value"]["enterprise_url"],
        "enterprise.example"
    );
    assert_eq!(
        document["credentials"]["github-copilot"]["extra"]["value"]["account_id"],
        "account-identity"
    );
    let reopened = FileCredentialStore::new(path);
    assert_eq!(
        block_on(reopened.read(ProviderId::new("github-copilot"), CancellationToken::new()))
            .unwrap(),
        Some(credential)
    );
}

#[test]
fn auth_file_store_local_adapter_round_trip() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let store = LocalFileCredentialStore::new(path.clone());
    let credential = Credential::ApiKey(ApiKeyCredential {
        key: Some(SecretString::new("local-secret")),
        environment: BTreeMap::new(),
    });

    block_on(async {
        let mut lease = LocalCredentialStore::acquire_lease(
            &store,
            ProviderId::new("local-provider"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        lease.replace(Some(credential.clone()));
        lease.commit().await.unwrap();
        assert_eq!(
            LocalCredentialStore::read(
                &store,
                ProviderId::new("local-provider"),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
            Some(credential)
        );
    });
    assert_eq!(store.path(), path);
}

#[test]
fn auth_file_store_rejects_unknown_schema() {
    let _basis = FILE_AUTH_BASIS;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    fs::write(&path, br#"{"schema_version":2,"credentials":{}}"#).unwrap();
    let store = FileCredentialStore::new(path);

    let error = block_on(store.list(CancellationToken::new())).unwrap_err();

    assert_eq!(error.code, "credential_schema");
    assert!(error.message.contains("unsupported schema version 2"));
}
