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
//! [`crate::auth::OAuthCredential::account_id`]), exactly as in pi.
//!
//! # Installation & scope (read before wiring)
//!
//! - **Provider-scoped only.** The resolver returns the Codex bearer for *every*
//!   request, ignoring the [`ModelIden`] it is handed. Install it on a
//!   `genai::Client` that only serves the Codex/ChatGPT provider (or otherwise
//!   scope it per provider); do **not** install it as a global resolver on a
//!   multi-provider client, or non-Codex providers would receive the Codex token.
//! - **Serialized refresh.** Refreshes are serialized (see
//!   [`CodexTokenResolver`]) so concurrent requests cannot double-refresh a
//!   rotating refresh token.
//! - **Synchronous store I/O.** The credential store performs blocking file I/O
//!   inside the async resolve. This is acceptable for a tiny single-file store
//!   (`~/.genai/auth.json`); the cross-process advisory-lock acquire, which can
//!   block on another process, is offloaded to a blocking thread.
//!
//! FORK-BRANCH CAVEAT: this feature pulls `genai` as a *path* dependency to
//! `../rust-genai`, which is currently parked on the
//! `feat/exec-interceptors-error-headers-tool-parts` branch. The default build of
//! this crate does not depend on genai at all; only `--features genai` does.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use fs2::FileExt;
use genai::resolver::{AuthData, AuthResolver};
use genai::ModelIden;
use tokio::sync::Mutex;

use crate::auth::codex::CodexAuth;
use crate::auth::credential::DEFAULT_EXPIRY_SKEW;
use crate::auth::error::{Error, Result};
use crate::auth::store::CredentialStore;

/// Build an async [`AuthResolver`] for a Codex credential.
///
/// The resolver is installable on a `genai::Client` via
/// `ClientBuilder::with_auth_resolver(...)`. On each resolve it:
/// 1. loads the credential for `provider_id`,
/// 2. if it is expired (with `skew` margin) and has a refresh token, refreshes it
///    (serialized — see [`CodexTokenResolver`]) and writes the new credential
///    back through `store`,
/// 3. returns `AuthData::from_single(access_token)`.
///
/// Uses [`DEFAULT_EXPIRY_SKEW`] via [`codex_auth_resolver`]; call
/// [`codex_auth_resolver_with_skew`] to customize the margin. See the
/// module docs for installation scope (provider-scoped only).
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
    let resolver = Arc::new(CodexTokenResolver::with_skew(
        auth,
        store,
        provider_id,
        skew,
    ));
    AuthResolver::from_resolver_async_fn(
        move |_model: ModelIden| -> Pin<
            Box<dyn Future<Output = genai::resolver::Result<Option<AuthData>>> + Send>,
        > {
            let resolver = resolver.clone();
            Box::pin(async move {
                let access = resolver
                    .resolve()
                    .await
                    .map_err(|e| genai::resolver::Error::Custom(e.to_string()))?;
                Ok(Some(AuthData::from_single(access)))
            })
        },
    )
}

/// Serialized, refresh-coordinating access-token resolver for a Codex credential.
///
/// This is the coordination object behind [`codex_auth_resolver`]. It exists to
/// fix a double-refresh race: without serialization, two concurrent genai
/// requests both observe the stored credential as expired, both POST the same
/// *rotating* refresh token, and the loser gets `invalid_grant`. pi avoids this
/// by running refresh inside a serialized per-provider `modify` lock
/// (types.ts:54-57, 86-90); this type reproduces that with:
///
/// - an in-process [`tokio::sync::Mutex`] guarding the whole
///   load-check-refresh-store sequence (one instance is provider-scoped, so a
///   single `Mutex<()>` suffices), and
/// - a best-effort **cross-process** OS advisory file lock (`flock`) on the
///   store's [`CredentialStore::lock_path`] sidecar, held across the same
///   sequence, so two processes sharing one `auth.json` also serialize.
///
/// Inside the lock it re-loads the credential and re-checks
/// [`OAuthCredential::is_expired`](crate::auth::OAuthCredential::is_expired), so a
/// waiter that acquires the lock *after* the winner refreshed observes the fresh
/// token and skips the refresh entirely (double-checked locking).
///
/// CROSS-PROCESS CAVEAT: cross-process serialization is best-effort and only
/// covers refreshes routed through this resolver. If the store exposes no
/// `lock_path`, or the lock file cannot be created/locked, only the in-process
/// mutex applies; and direct `store.store(...)` writes made outside the resolver
/// are not `flock`-serialized. A `CodexStreamFn`/application that mutates the
/// same credential from another path should serialize accordingly.
///
/// Usable directly (outside genai) by applications with custom wiring.
pub struct CodexTokenResolver {
    auth: Arc<CodexAuth>,
    store: Arc<dyn CredentialStore>,
    provider_id: String,
    skew: Duration,
    /// In-process serialization of the read-modify-write refresh.
    refresh_lock: Mutex<()>,
}

impl CodexTokenResolver {
    /// New resolver using [`DEFAULT_EXPIRY_SKEW`].
    pub fn new(
        auth: Arc<CodexAuth>,
        store: Arc<dyn CredentialStore>,
        provider_id: impl Into<String>,
    ) -> Self {
        Self::with_skew(auth, store, provider_id, DEFAULT_EXPIRY_SKEW)
    }

    /// New resolver with an explicit expiry skew margin.
    pub fn with_skew(
        auth: Arc<CodexAuth>,
        store: Arc<dyn CredentialStore>,
        provider_id: impl Into<String>,
        skew: Duration,
    ) -> Self {
        Self {
            auth,
            store,
            provider_id: provider_id.into(),
            skew,
            refresh_lock: Mutex::new(()),
        }
    }

    /// Resolve the current bearer access token, refreshing (once, serialized) if
    /// it is expired. Safe to call concurrently: at most one refresh request is
    /// issued per expiry window across all waiters (in-process, and cross-process
    /// where the store supports it).
    pub async fn resolve(&self) -> Result<String> {
        // In-process serialization: only one refresh per resolver at a time.
        let _inproc = self.refresh_lock.lock().await;

        // Cross-process (best-effort): hold an flock across the read-modify-write
        // so two processes sharing the same auth.json cannot both refresh. Held
        // for the whole double-checked sequence; released when the guard drops.
        let _xproc = match self.store.lock_path() {
            Some(path) => acquire_file_lock(path).await,
            None => None,
        };

        // Double-checked: `resolve_access_token` re-loads and re-checks expiry,
        // so a waiter that took the lock after the winner refreshed returns the
        // fresh token without issuing another refresh.
        resolve_access_token(
            &self.auth,
            self.store.as_ref(),
            &self.provider_id,
            self.skew,
        )
        .await
    }
}

/// Load, refresh-if-expired, persist, and return the current access token.
///
/// Public so applications with a custom store/wiring can reuse the exact
/// refresh-and-persist behavior outside the genai resolver.
///
/// NOTE: this building block is **not** internally serialized — it performs a
/// bare load → check → refresh → store with no lock across the `await`.
/// Concurrent callers that share a rotating refresh token must serialize
/// themselves; prefer [`CodexTokenResolver`] (or the [`codex_auth_resolver`]
/// it backs), which wraps this in an in-process mutex + cross-process file lock
/// with double-checked expiry.
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

/// RAII guard holding an exclusive OS advisory lock (`flock`) on the sidecar
/// lock file. The lock releases when the inner handle is dropped (its FD is
/// closed), which happens when this guard drops.
struct FileLockGuard {
    _file: std::fs::File,
}

/// Acquire an exclusive advisory lock on `lock_path` (best-effort).
///
/// The blocking `flock` acquire runs on a blocking thread (`spawn_blocking`) so
/// waiting on a *cross-process* lock never wedges an async worker. Returns
/// `None` if the lock file cannot be created/locked (in which case only the
/// in-process mutex serializes) — see the cross-process caveat on
/// [`CodexTokenResolver`].
async fn acquire_file_lock(lock_path: PathBuf) -> Option<FileLockGuard> {
    tokio::task::spawn_blocking(move || {
        // Best-effort: ensure the lock file's directory exists.
        if let Some(parent) = lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .ok()?;
        // Blocks until the cross-process lock is acquired.
        file.lock_exclusive().ok()?;
        Some(FileLockGuard { _file: file })
    })
    .await
    .ok()
    .flatten()
}
