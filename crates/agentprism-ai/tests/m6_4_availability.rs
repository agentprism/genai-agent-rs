//! §10.7 credential-scoped availability conformance.

use agentprism_ai::*;
use futures_executor::block_on;
use serde_json::value::RawValue;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

const API: &str = "availability-test-api";
const PROVIDER: &str = "github-copilot";
const AMBIENT_PROVIDER: &str = "ambient";
const MISSING_PROVIDER: &str = "missing";

fn model_for(provider: &str, id: &str) -> ModelDescriptor {
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, id),
            display_name: id.into(),
            base_url: Url::parse("https://availability.invalid/v1").unwrap(),
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
            api: ApiId::new(API),
            schema_version: 1,
            value: RawValue::from_string("{}".into()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

fn model(id: &str) -> ModelDescriptor {
    model_for(PROVIDER, id)
}

fn environment_provider_send(provider: &str, variable: &str) -> ProviderRegistration {
    ProviderRegistration::builder(provider)
        .auth(Arc::new(ProviderAuthResolver::new(
            Some(Arc::new(EnvironmentApiKeyAuth::new(
                format!("{provider} API key"),
                [variable],
            ))),
            None,
        )))
        .models(vec![model_for(provider, &format!("{provider}-model"))])
        .api(ApiId::new(API), Arc::new(NoopApi))
        .build()
        .unwrap()
}

fn environment_provider_local(provider: &str, variable: &str) -> LocalProviderRegistration {
    LocalProviderRegistration::builder(provider)
        .auth(Rc::new(LocalProviderAuthResolver::new(
            Some(Rc::new(EnvironmentApiKeyAuth::new(
                format!("{provider} API key"),
                [variable],
            ))),
            None,
        )))
        .models(vec![model_for(provider, &format!("{provider}-model"))])
        .api(ApiId::new(API), Rc::new(LocalNoopApi))
        .build()
        .unwrap()
}

fn entitlement_filter(
    models: &[ModelDescriptor],
    credential: Option<&Credential>,
) -> Vec<ModelDescriptor> {
    let Some(Credential::OAuth(OAuthCredential {
        extra:
            ProviderOAuthExtra::GitHubCopilot {
                available_model_ids: Some(available),
                ..
            },
        ..
    })) = credential
    else {
        return models.to_vec();
    };
    models
        .iter()
        .filter(|model| available.contains(&model.common.model_ref.model))
        .cloned()
        .collect()
}

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

struct NeverRefreshOAuth(Arc<AtomicUsize>);

impl OAuthAuth for NeverRefreshOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login(
        &self,
        _interaction: Arc<dyn AuthInteraction>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async { Err(AuthError::new("unexpected", "unexpected login")) })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
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
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

struct LocalNeverRefreshOAuth(Rc<Cell<usize>>);

impl LocalOAuthAuth for LocalNeverRefreshOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login(
        &self,
        _interaction: Rc<dyn LocalAuthInteraction>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async { Err(AuthError::new("unexpected", "unexpected login")) })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        self.0.set(self.0.get() + 1);
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
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("OAuth"),
            })
        })
    }
}

fn credential() -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new("expired-access"),
        refresh: SecretString::new("refresh"),
        expires_at: Timestamp::from_unix_millis(0),
        extra: ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: Url::parse("https://api.individual.githubcopilot.com").unwrap(),
            account_id: Some("account".into()),
            enterprise_url: None,
            available_model_ids: Some(vec![ModelId::new("entitled")]),
        },
    })
}

fn seed_send(store: &Arc<InMemoryCredentialStore>) {
    block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new(PROVIDER), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(credential()));
        lease.commit().await.unwrap();
    });
}

fn seed_local(store: &Rc<LocalInMemoryCredentialStore>) {
    block_on(async {
        let mut lease = store
            .acquire_lease(ProviderId::new(PROVIDER), CancellationToken::new())
            .await
            .unwrap();
        lease.replace(Some(credential()));
        lease.commit().await.unwrap();
    });
}

#[test]
fn credential_scoped_availability_send_pi_exact() {
    // §10.7; Pi basis: packages/ai/test/models-runtime.test.ts:806-837,
    // packages/ai/src/models.ts checkProviderAuth/checkAuth/getAvailable, and
    // providers/github-copilot.ts filterModels.
    let refreshes = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(InMemoryCredentialStore::default());
    seed_send(&store);
    let registration = ProviderRegistration::builder(PROVIDER)
        .auth(Arc::new(ProviderAuthResolver::new(
            None,
            Some(Arc::new(NeverRefreshOAuth(Arc::clone(&refreshes)))),
        )))
        .models(vec![model("entitled"), model("hidden")])
        .filter_models(Arc::new(entitlement_filter))
        .api(ApiId::new(API), Arc::new(NoopApi))
        .build()
        .unwrap();
    let models = Models::builder()
        .credential_store(store)
        .auth_context(Arc::new(MapAuthContext::new(
            BTreeMap::from([("AMBIENT_KEY".into(), "env-key".into())]),
            [],
        )))
        .provider(environment_provider_send(AMBIENT_PROVIDER, "AMBIENT_KEY"))
        .provider(environment_provider_send(MISSING_PROVIDER, "MISSING_KEY"))
        .provider(registration)
        .build()
        .unwrap();

    assert_eq!(models.models().len(), 4);
    let filtered = models.filter_models(&ProviderId::new(PROVIDER), Some(&credential()));
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].common.model_ref.model, ModelId::new("entitled"));
    let check = block_on(models.check_auth(ProviderId::new(PROVIDER), CancellationToken::new()))
        .unwrap()
        .unwrap();
    assert_eq!(check.credential_type, CredentialType::OAuth);
    assert_eq!(
        block_on(models.check_auth(ProviderId::new(AMBIENT_PROVIDER), CancellationToken::new()))
            .unwrap(),
        Some(AuthCheck {
            source: Some(AuthSource::new("AMBIENT_KEY")),
            credential_type: CredentialType::ApiKey,
        })
    );
    assert!(
        block_on(models.check_auth(ProviderId::new(MISSING_PROVIDER), CancellationToken::new()))
            .unwrap()
            .is_none()
    );
    let available = block_on(models.get_available(None, CancellationToken::new())).unwrap();
    assert_eq!(available.len(), 2);
    assert_eq!(
        available
            .iter()
            .map(|model| model.common.model_ref.provider.as_str())
            .collect::<Vec<_>>(),
        [AMBIENT_PROVIDER, PROVIDER]
    );
    assert_eq!(
        available[1].common.model_ref.model,
        ModelId::new("entitled")
    );
    let provider_available = block_on(models.get_available(
        Some(ProviderId::new(AMBIENT_PROVIDER)),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(provider_available.len(), 1);
    assert_eq!(
        provider_available[0].common.model_ref.provider,
        ProviderId::new(AMBIENT_PROVIDER)
    );
    assert!(
        block_on(models.get_available(
            Some(ProviderId::new(MISSING_PROVIDER)),
            CancellationToken::new()
        ))
        .unwrap()
        .is_empty()
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
}

#[test]
fn credential_scoped_availability_local_pi_exact() {
    // §10.7 local counterpart; same pinned Pi bases as the Send test,
    // including packages/ai/test/models-runtime.test.ts:806-837.
    let refreshes = Rc::new(Cell::new(0));
    let store = Rc::new(LocalInMemoryCredentialStore::default());
    seed_local(&store);
    let registration = LocalProviderRegistration::builder(PROVIDER)
        .auth(Rc::new(LocalProviderAuthResolver::new(
            None,
            Some(Rc::new(LocalNeverRefreshOAuth(Rc::clone(&refreshes)))),
        )))
        .models(vec![model("entitled"), model("hidden")])
        .filter_models(Rc::new(entitlement_filter))
        .api(ApiId::new(API), Rc::new(LocalNoopApi))
        .build()
        .unwrap();
    let models = LocalModels::builder()
        .credential_store(store)
        .auth_context(Rc::new(MapAuthContext::new(
            BTreeMap::from([("AMBIENT_KEY".into(), "env-key".into())]),
            [],
        )))
        .provider(environment_provider_local(AMBIENT_PROVIDER, "AMBIENT_KEY"))
        .provider(environment_provider_local(MISSING_PROVIDER, "MISSING_KEY"))
        .provider(registration)
        .build()
        .unwrap();

    assert_eq!(models.models().len(), 4);
    let filtered = models.filter_models(&ProviderId::new(PROVIDER), Some(&credential()));
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].common.model_ref.model, ModelId::new("entitled"));
    let check = block_on(models.check_auth(ProviderId::new(PROVIDER), CancellationToken::new()))
        .unwrap()
        .unwrap();
    assert_eq!(check.credential_type, CredentialType::OAuth);
    assert_eq!(
        block_on(models.check_auth(ProviderId::new(AMBIENT_PROVIDER), CancellationToken::new()))
            .unwrap(),
        Some(AuthCheck {
            source: Some(AuthSource::new("AMBIENT_KEY")),
            credential_type: CredentialType::ApiKey,
        })
    );
    assert!(
        block_on(models.check_auth(ProviderId::new(MISSING_PROVIDER), CancellationToken::new()))
            .unwrap()
            .is_none()
    );
    let available = block_on(models.get_available(None, CancellationToken::new())).unwrap();
    assert_eq!(available.len(), 2);
    assert_eq!(
        available
            .iter()
            .map(|model| model.common.model_ref.provider.as_str())
            .collect::<Vec<_>>(),
        [AMBIENT_PROVIDER, PROVIDER]
    );
    assert_eq!(
        available[1].common.model_ref.model,
        ModelId::new("entitled")
    );
    let provider_available = block_on(models.get_available(
        Some(ProviderId::new(AMBIENT_PROVIDER)),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(provider_available.len(), 1);
    assert_eq!(
        provider_available[0].common.model_ref.provider,
        ProviderId::new(AMBIENT_PROVIDER)
    );
    assert!(
        block_on(models.get_available(
            Some(ProviderId::new(MISSING_PROVIDER)),
            CancellationToken::new()
        ))
        .unwrap()
        .is_empty()
    );
    assert_eq!(refreshes.get(), 0);
}
