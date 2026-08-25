#![cfg(target_arch = "wasm32")]

use futures_util::future::{join, pending};
use pi_ai::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const WASM_AUTH_BASIS: &str = "architecture v2 part 2 §9.2, §10.7; packages/ai/src/auth/credential-store.ts:1-67; packages/ai/src/auth/resolve.ts:86-168";
const WASM_DEVICE_BASIS: &str =
    "architecture v2 part 2 §6.1, §9.2, §10.7; packages/ai/src/auth/oauth/device-code.ts:27-98";

#[derive(Clone, Copy)]
struct FixedLocalClock;

impl LocalAuthClock for FixedLocalClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(1_000)
    }
}

struct DelayedLocalOAuth {
    refreshes: Rc<Cell<usize>>,
}

impl LocalOAuthAuth for DelayedLocalOAuth {
    fn name(&self) -> &str {
        "Pending browser OAuth"
    }

    fn login(
        &self,
        _interaction: Rc<dyn LocalAuthInteraction>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async { pending().await })
    }

    fn refresh(
        &self,
        _credential: OAuthCredential,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        self.refreshes.set(self.refreshes.get() + 1);
        Box::pin(async {
            futures_timer::Delay::new(Duration::from_millis(1)).await;
            Ok(OAuthCredential {
                access: SecretString::new("refreshed-access"),
                refresh: SecretString::new("rotated-refresh"),
                expires_at: Timestamp::from_unix_millis(4_000_000),
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

fn expired_oauth_credential() -> Credential {
    Credential::OAuth(OAuthCredential {
        access: SecretString::new("expired-access"),
        refresh: SecretString::new("refresh-secret"),
        expires_at: Timestamp::from_unix_millis(0),
        extra: ProviderOAuthExtra::None,
    })
}

#[wasm_bindgen_test]
async fn auth_device_poll_is_cancellable() {
    let _basis = WASM_DEVICE_BASIS;
    let cancellation = CancellationToken::new();
    let cancel_after_tick = {
        let cancellation = cancellation.clone();
        async move {
            futures_timer::Delay::new(Duration::from_millis(1)).await;
            cancellation.cancel();
        }
    };
    let sleep = LocalOAuthDeviceCodeRuntime::sleep(
        &SystemOAuthDeviceCodeRuntime,
        Duration::from_secs(30),
        cancellation,
    );

    let (result, ()) = futures_util::future::join(sleep, cancel_after_tick).await;
    assert_eq!(result, Err(AuthError::Cancelled));
}

#[wasm_bindgen_test]
async fn auth_oauth_refresh_is_serialized() {
    let _basis = WASM_AUTH_BASIS;
    let store = Rc::new(LocalInMemoryCredentialStore::new());
    let provider_id = ProviderId::new("browser-oauth");
    let mut lease = LocalCredentialStore::acquire_lease(
        store.as_ref(),
        provider_id.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    lease.replace(Some(expired_oauth_credential()));
    lease.commit().await.unwrap();

    let refreshes = Rc::new(Cell::new(0));
    let resolver = LocalProviderAuthResolver::new(
        None,
        Some(Rc::new(DelayedLocalOAuth {
            refreshes: Rc::clone(&refreshes),
        })),
    )
    .with_clock(Rc::new(FixedLocalClock));
    let provider = LocalProviderRegistration::builder(provider_id.as_str())
        .auth(Rc::new(resolver))
        .build()
        .unwrap();
    let models = LocalModels::builder()
        .credential_store(store)
        .provider(provider)
        .build()
        .unwrap();

    let cancellation = CancellationToken::new();
    let first = models.resolve_auth(
        provider_id.clone(),
        AuthResolutionOverrides::default(),
        cancellation.clone(),
    );
    let second = models.resolve_auth(
        provider_id,
        AuthResolutionOverrides::default(),
        cancellation,
    );

    let (first, second) = join(first, second).await;
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_eq!(
        first.api_key.as_ref().unwrap().expose_secret(),
        "refreshed-access"
    );
    assert_eq!(
        second.api_key.as_ref().unwrap().expose_secret(),
        "refreshed-access"
    );
    assert_eq!(refreshes.get(), 1);
}
