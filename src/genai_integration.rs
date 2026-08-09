//! genai integration (feature = `genai`).
//!
//! Produces a [`genai::resolver::AuthResolver`] that, on each request, loads the
//! stored Codex credential, refreshes it if it is expired (persisting the fresh
//! token via the [`CredentialStore`]), and returns the bearer as
//! [`genai::resolver::AuthData`]. This keeps genai and the application
//! auth-agnostic — the resolver is the only integration seam.
//!
//! This mirrors pi's `OAuthAuth.toAuth(credential) => { apiKey: credential.access }`
//! (openai-codex.ts:541-543): the resolved value is the bearer access token. Any
//! provider-specific wiring that ChatGPT's backend also needs — the
//! `chatgpt-account-id` header and the `chatgpt.com/backend-api/codex` base URL —
//! is the application's responsibility (the account id is available on the stored
//! [`crate::OAuthCredential::account_id`]), exactly as in pi.
//!
//! FORK-BRANCH CAVEAT: this feature pulls `genai` as a *path* dependency to
//! `../rust-genai`, which is currently parked on the
//! `feat/exec-interceptors-error-headers-tool-parts` branch. The default build of
//! this crate does not depend on genai at all; only `--features genai` does.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use genai::resolver::{AuthData, AuthResolver};
use genai::ModelIden;

use crate::codex::CodexAuth;
use crate::credential::DEFAULT_EXPIRY_SKEW;
use crate::error::{Error, Result};
use crate::store::CredentialStore;

/// Build an async [`AuthResolver`] for a Codex credential.
///
/// The resolver is installable on a `genai::Client` via
/// `ClientBuilder::with_auth_resolver(...)`. On each resolve it:
/// 1. loads the credential for `provider_id`,
/// 2. if it is expired (with `skew` margin) and has a refresh token, refreshes it
///    and writes the new credential back through `store`,
/// 3. returns `AuthData::from_single(access_token)`.
///
/// Uses [`DEFAULT_EXPIRY_SKEW`] via [`codex_auth_resolver`]; call
/// [`codex_auth_resolver_with_skew`] to customize the margin.
pub fn codex_auth_resolver(
    auth: Arc<CodexAuth>,
    store: Arc<dyn CredentialStore>,
    provider_id: impl Into<String>,
) -> AuthResolver {
    codex_auth_resolver_with_skew(auth, store, provider_id, DEFAULT_EXPIRY_SKEW)
}

/// Like [`codex_auth_resolver`] with an explicit expiry skew margin.
pub fn codex_auth_resolver_with_skew(
    auth: Arc<CodexAuth>,
    store: Arc<dyn CredentialStore>,
    provider_id: impl Into<String>,
    skew: Duration,
) -> AuthResolver {
    let provider_id = provider_id.into();
    AuthResolver::from_resolver_async_fn(
        move |_model: ModelIden| -> Pin<
            Box<dyn Future<Output = genai::resolver::Result<Option<AuthData>>> + Send>,
        > {
            let auth = auth.clone();
            let store = store.clone();
            let provider_id = provider_id.clone();
            Box::pin(async move {
                let access = resolve_access_token(&auth, store.as_ref(), &provider_id, skew)
                    .await
                    .map_err(|e| genai::resolver::Error::Custom(e.to_string()))?;
                Ok(Some(AuthData::from_single(access)))
            })
        },
    )
}

/// Load, refresh-if-expired, persist, and return the current access token.
///
/// Public so applications with a custom store/wiring can reuse the exact
/// refresh-and-persist behavior outside the genai resolver.
pub async fn resolve_access_token(
    auth: &CodexAuth,
    store: &dyn CredentialStore,
    provider_id: &str,
    skew: Duration,
) -> Result<String> {
    let current = store
        .load(provider_id)?
        .ok_or_else(|| Error::NoCredential(provider_id.to_string()))?;

    if current.is_expired(skew) && current.refresh_token.is_some() {
        let refreshed = auth.refresh(&current).await?;
        store.store(provider_id, &refreshed)?;
        return Ok(refreshed.access_token);
    }

    Ok(current.access_token)
}
