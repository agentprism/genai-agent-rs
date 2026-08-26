//! Cloudflare Workers AI auth precedence conformance.

use agentprism_ai::*;
use agentprism_cloudflare_workers_ai::*;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy)]
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

fn credential() -> Credential {
    Credential::ApiKey(ApiKeyCredential {
        key: Some(SecretString::new("stored-key")),
        environment: BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".into(), "stored-account".into())]),
    })
}

fn request_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".into(), "request-account".into())])
}

mod send {
    use super::*;

    #[test]
    fn auth_explicit_request_value_wins() {
        // Architecture v2 part 2 §6.1/§10.7; Pi basis: models.ts merges
        // request env after stored env, and providers/cloudflare-auth.ts reads
        // the merged credential field before ambient environment.
        let registration = provider(ProviderInputs {
            http: Arc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .unwrap();
        let store = Arc::new(InMemoryCredentialStore::new());
        futures_executor::block_on(async {
            let mut lease = store
                .acquire_lease(
                    ProviderId::new("cloudflare-workers-ai"),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            lease.replace(Some(credential()));
            lease.commit().await.unwrap();
        });
        let mut request = ResolveAuthRequest::isolated(
            registration.descriptor.clone(),
            Some(registration.catalog.snapshot()[0].clone()),
        );
        request.credential_store = store;
        request.overrides.environment = request_environment();
        let resolved = futures_executor::block_on(
            registration.auth.resolve(request, CancellationToken::new()),
        )
        .unwrap()
        .unwrap();
        assert!(
            resolved
                .base_url
                .unwrap()
                .as_str()
                .contains("/accounts/request-account/ai/v1")
        );
    }
}

mod local {
    use super::*;

    #[test]
    fn auth_explicit_request_value_wins() {
        // Local §9.2 counterpart to the Send conformance test above.
        let registration = local_provider(LocalProviderInputs {
            http: Rc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .unwrap();
        let store = Rc::new(LocalInMemoryCredentialStore::new());
        futures_executor::block_on(async {
            let mut lease = LocalCredentialStore::acquire_lease(
                store.as_ref(),
                ProviderId::new("cloudflare-workers-ai"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            lease.replace(Some(credential()));
            lease.commit().await.unwrap();
        });
        let mut request = LocalResolveAuthRequest::isolated(
            registration.descriptor.clone(),
            Some(registration.catalog.snapshot()[0].clone()),
        );
        request.credential_store = store;
        request.overrides.environment = request_environment();
        let resolved = futures_executor::block_on(
            registration.auth.resolve(request, CancellationToken::new()),
        )
        .unwrap()
        .unwrap();
        assert!(
            resolved
                .base_url
                .unwrap()
                .as_str()
                .contains("/accounts/request-account/ai/v1")
        );
    }
}
