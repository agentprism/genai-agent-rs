//! Authentication contracts, credential transactions, and provider-owned
//! resolution from Architecture v2 part 1 §3.8 and part 2 §6.1–§6.6.

use crate::{
    AuthChallengeId, AuthResolutionPurpose, AuthSource, CancellationToken, ExtensionId,
    LocalBoxFuture, ProviderDescriptor, ProviderId, ResolvedAuth, SendBoxFuture, StoreError,
    Timestamp,
};
use futures_util::future::{Either, select};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// Secret UTF-8 data that always redacts its debug representation and has no
/// general-purpose serialization implementation.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a secret string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicitly exposes the secret to authentication or transport code.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and exposes its secret value.
    pub fn into_secret(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// Stored API-key credential. Environment values are provider-scoped
/// configuration used while deriving an effective endpoint or headers.
#[derive(Clone, Eq, PartialEq)]
pub struct ApiKeyCredential {
    /// Optional provider API key. Ambient-only providers may store only
    /// provider-scoped environment values.
    pub key: Option<SecretString>,
    /// Provider-scoped environment/configuration values.
    pub environment: BTreeMap<String, String>,
}

impl fmt::Debug for ApiKeyCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiKeyCredential")
            .field("key", &self.key)
            .field("environment", &"[REDACTED PROVIDER ENVIRONMENT]")
            .finish()
    }
}

/// Canonical in-memory OAuth credential (Architecture v2 part 2 §6.6).
///
/// This type intentionally has no general-purpose serde implementation. A
/// persistent credential store must choose an explicit protected format.
#[derive(Clone, Eq, PartialEq)]
pub struct OAuthCredential {
    /// Current access token.
    pub access: SecretString,
    /// Refresh token used for rotation.
    pub refresh: SecretString,
    /// Absolute access-token expiry in Unix milliseconds.
    pub expires_at: Timestamp,
    /// Typed provider-specific noncanonical fields.
    pub extra: ProviderOAuthExtra,
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("access", &self.access)
            .field("refresh", &self.refresh)
            .field("expires_at", &self.expires_at)
            .field("extra", &"[REDACTED CREDENTIAL EXTRA]")
            .finish()
    }
}

/// Typed provider-owned OAuth fields (Architecture v2 part 2 §6.6).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", content = "value")]
pub enum ProviderOAuthExtra {
    /// No provider-specific fields.
    None,
    /// Radius gateway selection.
    Radius {
        /// Credential-specific gateway URL.
        gateway_url: Url,
        /// Optional Radius organization identifier.
        organization_id: Option<String>,
    },
    /// GitHub Copilot endpoint and account identity.
    GitHubCopilot {
        /// Credential-specific API endpoint.
        api_endpoint: Url,
        /// Optional account identifier.
        account_id: Option<String>,
    },
    /// OpenAI Codex account identity.
    OpenAiCodex {
        /// Required account identifier.
        account_id: String,
    },
    /// Third-party typed credential data.
    Custom {
        /// Extension schema owner.
        schema: ExtensionId,
        /// Extension schema version.
        schema_version: u32,
        /// Exact custom JSON value.
        value: Box<RawValue>,
    },
}

impl PartialEq for ProviderOAuthExtra {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (
                Self::Radius {
                    gateway_url: left_url,
                    organization_id: left_organization,
                },
                Self::Radius {
                    gateway_url: right_url,
                    organization_id: right_organization,
                },
            ) => left_url == right_url && left_organization == right_organization,
            (
                Self::GitHubCopilot {
                    api_endpoint: left_endpoint,
                    account_id: left_account,
                },
                Self::GitHubCopilot {
                    api_endpoint: right_endpoint,
                    account_id: right_account,
                },
            ) => left_endpoint == right_endpoint && left_account == right_account,
            (
                Self::OpenAiCodex {
                    account_id: left_account,
                },
                Self::OpenAiCodex {
                    account_id: right_account,
                },
            ) => left_account == right_account,
            (
                Self::Custom {
                    schema: left_schema,
                    schema_version: left_version,
                    value: left_value,
                },
                Self::Custom {
                    schema: right_schema,
                    schema_version: right_version,
                    value: right_value,
                },
            ) => {
                left_schema == right_schema
                    && left_version == right_version
                    && left_value.get() == right_value.get()
            }
            _ => false,
        }
    }
}

impl Eq for ProviderOAuthExtra {}

/// One type-tagged credential per provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Credential {
    /// API-key or ambient provider configuration.
    ApiKey(ApiKeyCredential),
    /// OAuth access/refresh credential.
    OAuth(OAuthCredential),
}

impl Credential {
    /// Returns the non-secret credential category.
    pub const fn credential_type(&self) -> CredentialType {
        match self {
            Self::ApiKey(_) => CredentialType::ApiKey,
            Self::OAuth(_) => CredentialType::OAuth,
        }
    }
}

/// Non-secret credential category used by account/status UIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    /// API-key credential.
    ApiKey,
    /// OAuth credential.
    OAuth,
}

/// Non-secret metadata returned by [`CredentialStore::list`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialInfo {
    /// Provider that owns the credential.
    pub provider: ProviderId,
    /// Credential category; no credential values are exposed.
    pub credential_type: CredentialType,
}

/// Serialized read-modify-write lease from Architecture v2 part 1 §3.8.
pub trait CredentialLease: Send + 'static {
    /// Returns the credential observed while acquiring the lease.
    fn current(&self) -> Option<&Credential>;

    /// Stages a replacement. `None` stages deletion.
    fn replace(&mut self, credential: Option<Credential>);

    /// Atomically publishes the staged value and releases the lease.
    fn commit(self: Box<Self>) -> SendBoxFuture<'static, Result<(), StoreError>>;
}

/// Object-safe credential storage with provider-scoped write serialization.
pub trait CredentialStore: Send + Sync + 'static {
    /// Reads one credential without resolving or refreshing it.
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<Credential>, StoreError>>;

    /// Lists non-secret credential metadata.
    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>>;

    /// Acquires exclusive provider-scoped read-modify-write ownership.
    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn CredentialLease>, StoreError>>;
}

/// Local-executor counterpart to [`CredentialLease`].
pub trait LocalCredentialLease: 'static {
    /// Returns the credential observed while acquiring the lease.
    fn current(&self) -> Option<&Credential>;

    /// Stages a replacement. `None` stages deletion.
    fn replace(&mut self, credential: Option<Credential>);

    /// Atomically publishes the staged value and releases the lease.
    fn commit(self: Box<Self>) -> LocalBoxFuture<'static, Result<(), StoreError>>;
}

/// Local-executor counterpart to [`CredentialStore`].
pub trait LocalCredentialStore: 'static {
    /// Reads one credential.
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<Credential>, StoreError>>;

    /// Lists non-secret credential metadata.
    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>>;

    /// Acquires exclusive provider-scoped read-modify-write ownership.
    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalCredentialLease>, StoreError>>;
}

/// Process-local in-memory credential store with FIFO leases per provider.
#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    inner: Arc<Mutex<InMemoryCredentialState>>,
}

#[derive(Default)]
struct InMemoryCredentialState {
    credentials: BTreeMap<ProviderId, Credential>,
    locks: BTreeMap<ProviderId, ProviderLeaseState>,
    next_waiter: u64,
}

#[derive(Default)]
struct ProviderLeaseState {
    held: bool,
    waiters: VecDeque<(u64, Waker)>,
}

impl fmt::Debug for InMemoryCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryCredentialStore")
            .field(
                "credential_count",
                &lock_unpoisoned(&self.inner).credentials.len(),
            )
            .finish_non_exhaustive()
    }
}

impl InMemoryCredentialStore {
    /// Creates an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience read-modify-write operation for concrete Rust callers.
    pub async fn modify<F, Fut>(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
        modify: F,
    ) -> Result<Option<Credential>, StoreError>
    where
        F: FnOnce(Option<&Credential>) -> Fut,
        Fut: Future<Output = Result<Option<Credential>, StoreError>>,
    {
        let mut lease = CredentialStore::acquire_lease(self, provider, cancellation).await?;
        let next = modify(lease.current()).await?;
        lease.replace(next.clone());
        lease.commit().await?;
        Ok(next)
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<Credential>, StoreError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "credential operation cancelled"))?;
            Ok(lock_unpoisoned(&inner).credentials.get(&provider).cloned())
        })
    }

    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| StoreError::new("cancelled", "credential operation cancelled"))?;
            Ok(lock_unpoisoned(&inner)
                .credentials
                .iter()
                .map(|(provider, credential)| CredentialInfo {
                    provider: provider.clone(),
                    credential_type: credential.credential_type(),
                })
                .collect())
        })
    }

    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn CredentialLease>, StoreError>> {
        let store = self.clone();
        Box::pin(async move {
            let acquire = Box::pin(AcquireCredentialLease::new(
                store,
                provider,
                cancellation.clone(),
            ));
            let cancelled = Box::pin(cancellation.cancelled());
            match select(acquire, cancelled).await {
                Either::Left((result, _)) => {
                    result.map(|lease| Box::new(lease) as Box<dyn CredentialLease>)
                }
                Either::Right(((), _)) => Err(StoreError::new(
                    "cancelled",
                    "credential operation cancelled",
                )),
            }
        })
    }
}

struct AcquireCredentialLease {
    store: InMemoryCredentialStore,
    provider: ProviderId,
    cancellation: CancellationToken,
    waiter: Option<u64>,
    complete: bool,
}

impl AcquireCredentialLease {
    fn new(
        store: InMemoryCredentialStore,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            store,
            provider,
            cancellation,
            waiter: None,
            complete: false,
        }
    }
}

impl Future for AcquireCredentialLease {
    type Output = Result<InMemoryCredentialLease, StoreError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.cancellation.is_cancelled() {
            return Poll::Ready(Err(StoreError::new(
                "cancelled",
                "credential operation cancelled",
            )));
        }

        let store = self.store.clone();
        let provider = self.provider.clone();
        let waiter = self.waiter;
        let mut state = lock_unpoisoned(&store.inner);
        let can_acquire = {
            let provider_lock = state.locks.entry(provider.clone()).or_default();
            !provider_lock.held
                && match waiter {
                    Some(waiter) => {
                        provider_lock.waiters.front().map(|entry| entry.0) == Some(waiter)
                    }
                    None => provider_lock.waiters.is_empty(),
                }
        };

        if can_acquire {
            let provider_lock = state
                .locks
                .get_mut(&provider)
                .expect("provider lock was inserted above");
            provider_lock.held = true;
            if waiter.is_some() {
                provider_lock.waiters.pop_front();
            }
            let current = state.credentials.get(&provider).cloned();
            drop(state);
            self.complete = true;
            return Poll::Ready(Ok(InMemoryCredentialLease {
                store,
                provider,
                current,
                replacement: None,
                released: false,
            }));
        }

        let waiter = if let Some(waiter) = waiter {
            waiter
        } else {
            state.next_waiter = state.next_waiter.wrapping_add(1).max(1);
            let waiter = state.next_waiter;
            self.waiter = Some(waiter);
            waiter
        };
        let provider_lock = state.locks.entry(provider).or_default();
        match provider_lock
            .waiters
            .iter_mut()
            .find(|entry| entry.0 == waiter)
        {
            Some((_, waker)) if !waker.will_wake(context.waker()) => {
                *waker = context.waker().clone();
            }
            Some(_) => {}
            None => provider_lock
                .waiters
                .push_back((waiter, context.waker().clone())),
        }
        Poll::Pending
    }
}

impl Drop for AcquireCredentialLease {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let Some(waiter) = self.waiter else {
            return;
        };
        let mut state = lock_unpoisoned(&self.store.inner);
        if let Some(provider_lock) = state.locks.get_mut(&self.provider) {
            let was_front = provider_lock.waiters.front().map(|entry| entry.0) == Some(waiter);
            provider_lock.waiters.retain(|entry| entry.0 != waiter);
            if was_front
                && !provider_lock.held
                && let Some((_, waker)) = provider_lock.waiters.front()
            {
                waker.wake_by_ref();
            }
        }
    }
}

struct InMemoryCredentialLease {
    store: InMemoryCredentialStore,
    provider: ProviderId,
    current: Option<Credential>,
    replacement: Option<Option<Credential>>,
    released: bool,
}

impl InMemoryCredentialLease {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        release_provider_lease(&self.store.inner, &self.provider);
    }
}

impl CredentialLease for InMemoryCredentialLease {
    fn current(&self) -> Option<&Credential> {
        self.current.as_ref()
    }

    fn replace(&mut self, credential: Option<Credential>) {
        self.replacement = Some(credential);
    }

    fn commit(mut self: Box<Self>) -> SendBoxFuture<'static, Result<(), StoreError>> {
        Box::pin(async move {
            if let Some(replacement) = self.replacement.take() {
                let mut state = lock_unpoisoned(&self.store.inner);
                match replacement {
                    Some(credential) => {
                        state.credentials.insert(self.provider.clone(), credential);
                    }
                    None => {
                        state.credentials.remove(&self.provider);
                    }
                }
            }
            self.release();
            Ok(())
        })
    }
}

impl Drop for InMemoryCredentialLease {
    fn drop(&mut self) {
        self.release();
    }
}

fn release_provider_lease(inner: &Arc<Mutex<InMemoryCredentialState>>, provider: &ProviderId) {
    let mut state = lock_unpoisoned(inner);
    if let Some(provider_lock) = state.locks.get_mut(provider) {
        provider_lock.held = false;
        if let Some((_, waker)) = provider_lock.waiters.front() {
            waker.wake_by_ref();
        }
    }
}

/// Local-executor adapter around [`InMemoryCredentialStore`].
#[derive(Clone, Debug, Default)]
pub struct LocalInMemoryCredentialStore {
    inner: InMemoryCredentialStore,
}

impl LocalInMemoryCredentialStore {
    /// Creates an empty local store.
    pub fn new() -> Self {
        Self::default()
    }
}

struct LocalCredentialLeaseAdapter {
    inner: Option<Box<dyn CredentialLease>>,
}

impl LocalCredentialLease for LocalCredentialLeaseAdapter {
    fn current(&self) -> Option<&Credential> {
        self.inner.as_deref().and_then(CredentialLease::current)
    }

    fn replace(&mut self, credential: Option<Credential>) {
        self.inner
            .as_deref_mut()
            .expect("local credential lease is live")
            .replace(credential);
    }

    fn commit(mut self: Box<Self>) -> LocalBoxFuture<'static, Result<(), StoreError>> {
        let lease = self.inner.take().expect("local credential lease is live");
        Box::pin(async move { lease.commit().await })
    }
}

impl LocalCredentialStore for LocalInMemoryCredentialStore {
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<Credential>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move { CredentialStore::read(&store, provider, cancellation).await })
    }

    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move { CredentialStore::list(&store, cancellation).await })
    }

    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalCredentialLease>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move {
            let lease = CredentialStore::acquire_lease(&store, provider, cancellation).await?;
            Ok(Box::new(LocalCredentialLeaseAdapter { inner: Some(lease) })
                as Box<dyn LocalCredentialLease>)
        })
    }
}

/// Environment and ambient-file access used during auth resolution.
pub trait AuthContext: Send + Sync + 'static {
    /// Reads a nonempty environment/configuration value.
    fn env(
        &self,
        name: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<String>, AuthError>>;

    /// Checks an ambient credential file path.
    fn file_exists(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<bool, AuthError>>;

    /// Reads an ambient UTF-8 credential file without exposing its contents
    /// through ordinary model or provider configuration or debug output.
    ///
    /// The default preserves compatibility for hosts that only support
    /// existence checks. Providers that must authenticate from file contents
    /// report an explicit auth error when this capability is unavailable.
    fn read_file(
        &self,
        _path: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<SecretString>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(None)
        })
    }
}

/// Local-executor counterpart to [`AuthContext`].
pub trait LocalAuthContext: 'static {
    /// Reads a nonempty environment/configuration value.
    fn env(
        &self,
        name: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<String>, AuthError>>;

    /// Checks an ambient credential file path.
    fn file_exists(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<bool, AuthError>>;

    /// Local-executor counterpart to [`AuthContext::read_file`].
    fn read_file(
        &self,
        _path: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<SecretString>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(None)
        })
    }
}

/// Empty, portable default auth context. Native applications inject process
/// environment and filesystem capabilities explicitly.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyAuthContext;

impl AuthContext for EmptyAuthContext {
    fn env(
        &self,
        _name: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<String>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(None)
        })
    }

    fn file_exists(
        &self,
        _path: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<bool, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(false)
        })
    }
}

impl LocalAuthContext for EmptyAuthContext {
    fn env(
        &self,
        _name: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<String>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(None)
        })
    }

    fn file_exists(
        &self,
        _path: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<bool, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(false)
        })
    }
}

/// Deterministic in-memory auth context for hosts and hermetic tests.
#[derive(Clone, Default)]
pub struct MapAuthContext {
    environment: Arc<BTreeMap<String, String>>,
    files: Arc<BTreeSet<String>>,
    file_contents: Arc<BTreeMap<String, SecretString>>,
}

impl fmt::Debug for MapAuthContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MapAuthContext")
            .field("environment", &"[REDACTED]")
            .field("file_count", &self.files.len())
            .finish()
    }
}

impl MapAuthContext {
    /// Creates a context from provider-scoped values and existing paths.
    pub fn new(
        environment: BTreeMap<String, String>,
        files: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            environment: Arc::new(environment),
            files: Arc::new(files.into_iter().collect()),
            file_contents: Arc::new(BTreeMap::new()),
        }
    }

    /// Adds deterministic, redacted credential-file contents for hermetic auth
    /// flows.
    pub fn with_file(mut self, path: impl Into<String>, contents: SecretString) -> Self {
        let path = path.into();
        Arc::make_mut(&mut self.files).insert(path.clone());
        Arc::make_mut(&mut self.file_contents).insert(path, contents);
        self
    }
}

impl AuthContext for MapAuthContext {
    fn env(
        &self,
        name: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<String>, AuthError>> {
        let values = Arc::clone(&self.environment);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(values
                .get(&name)
                .filter(|value| !value.trim().is_empty())
                .cloned())
        })
    }

    fn file_exists(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<bool, AuthError>> {
        let files = Arc::clone(&self.files);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(files.contains(&path))
        })
    }

    fn read_file(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<SecretString>, AuthError>> {
        let files = Arc::clone(&self.file_contents);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(files.get(&path).cloned())
        })
    }
}

impl LocalAuthContext for MapAuthContext {
    fn env(
        &self,
        name: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<String>, AuthError>> {
        let values = Arc::clone(&self.environment);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(values
                .get(&name)
                .filter(|value| !value.trim().is_empty())
                .cloned())
        })
    }

    fn file_exists(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<bool, AuthError>> {
        let files = Arc::clone(&self.files);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(files.contains(&path))
        })
    }

    fn read_file(
        &self,
        path: String,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<SecretString>, AuthError>> {
        let files = Arc::clone(&self.file_contents);
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(files.get(&path).cloned())
        })
    }
}

/// Capabilities an authentication host can provide (part 2 §6.2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthHostCapabilities {
    /// Host can open an external browser.
    pub external_browser: bool,
    /// Host can receive loopback HTTP redirects.
    pub loopback_http: bool,
    /// Host can receive application URL schemes.
    pub custom_url_scheme: bool,
    /// Host can receive universal links.
    pub universal_links: bool,
    /// Host can accept manually pasted codes or redirect URLs.
    pub manual_paste: bool,
    /// Host can integrate with the platform clipboard.
    pub clipboard: bool,
}

/// One option shown by an authentication selection prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthSelectOption {
    /// Stable value returned to the flow.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Optional explanatory detail.
    pub description: Option<String>,
}

/// Host-rendered prompt (part 2 §6.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthPrompt {
    /// Ordinary text input.
    Text {
        /// Prompt text.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
    },
    /// Secret input that the host must not echo or log.
    Secret {
        /// Prompt text.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
    },
    /// Select one stable option ID.
    Select {
        /// Prompt text.
        message: String,
        /// Available options in display order.
        options: Vec<AuthSelectOption>,
    },
    /// Manually entered authorization code or redirect URL.
    ManualCode {
        /// Prompt text.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
        /// Host-visible challenge identifier.
        challenge_id: AuthChallengeId,
    },
}

/// Host answer to an authentication prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthAnswer {
    /// Text or secret input. Secret answers are immediately wrapped by the
    /// consuming flow and must not be persisted as this enum.
    Text(String),
    /// Stable ID from an [`AuthPrompt::Select`] option.
    Selected(String),
}

/// Informational link shown by the auth host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthInfoLink {
    /// Destination URL.
    pub url: Url,
    /// Optional display label.
    pub label: Option<String>,
}

/// Out-of-band auth event emitted to a host (part 2 §6.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthEvent {
    /// Informational message and optional links.
    Info {
        /// Human-readable information.
        message: String,
        /// Related links.
        links: Vec<AuthInfoLink>,
    },
    /// Request to open an authorization URL.
    OpenUrl {
        /// Challenge associated with the URL.
        challenge_id: AuthChallengeId,
        /// Authorization URL.
        url: Url,
        /// Optional provider instructions.
        instructions: Option<String>,
    },
    /// RFC 8628 device-code challenge.
    DeviceCode {
        /// Challenge identifier.
        challenge_id: AuthChallengeId,
        /// Short code displayed to the user.
        user_code: String,
        /// Provider verification URL.
        verification_uri: Url,
        /// Provider polling interval.
        interval: Option<Duration>,
        /// Provider challenge lifetime.
        expires_in: Option<Duration>,
    },
    /// Progress text.
    Progress {
        /// Human-readable progress message.
        message: String,
    },
}

/// Failure at the host interaction boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthInteractionError {
    /// Host cancelled the whole flow or one prompt.
    Cancelled,
    /// A response arrived after another valid response closed the challenge.
    ChallengeSuperseded {
        /// Closed challenge identifier.
        challenge_id: AuthChallengeId,
    },
    /// Host does not implement a requested operation.
    Unsupported {
        /// Sanitized explanation.
        message: String,
    },
    /// Host operation failed.
    Failed {
        /// Stable host error code.
        code: String,
        /// Sanitized explanation.
        message: String,
    },
}

impl fmt::Display for AuthInteractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("authentication interaction cancelled"),
            Self::ChallengeSuperseded { challenge_id } => {
                write!(
                    formatter,
                    "authentication challenge {challenge_id} was superseded"
                )
            }
            Self::Unsupported { message } | Self::Failed { message, .. } => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for AuthInteractionError {}

/// Send host contract for prompts, notifications, and redirect reception.
pub trait AuthInteraction: Send + Sync + 'static {
    /// Reports platform capabilities synchronously.
    fn capabilities(&self) -> AuthHostCapabilities;

    /// Requests one answer. Cancellation may close a losing prompt.
    fn prompt(
        &self,
        prompt: AuthPrompt,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>>;

    /// Emits a nonblocking informational event.
    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError>;

    /// Creates a host-owned redirect receiver.
    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>>;
}

/// Local-executor host contract corresponding to [`AuthInteraction`].
pub trait LocalAuthInteraction: 'static {
    /// Reports platform capabilities synchronously.
    fn capabilities(&self) -> AuthHostCapabilities;

    /// Requests one answer.
    fn prompt(
        &self,
        prompt: AuthPrompt,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>>;

    /// Emits a nonblocking informational event.
    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError>;

    /// Creates a local host-owned redirect receiver.
    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRedirectReceiver>, AuthInteractionError>>;
}

/// Browser callback page supplied to a host receiver.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthHtmlPage {
    /// HTML document bytes represented as UTF-8.
    pub html: String,
}

/// Redirect strategy the provider flow can accept (part 2 §6.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RedirectStrategy {
    /// Fixed host, port, and path required by a provider registration.
    FixedLoopback {
        /// Loopback address.
        host: IpAddr,
        /// Required port.
        port: u16,
        /// Required callback path.
        path: String,
    },
    /// Host chooses an available loopback port.
    EphemeralLoopback {
        /// Loopback address.
        host: IpAddr,
        /// Callback path.
        path: String,
    },
    /// Application-specific URL scheme.
    CustomScheme {
        /// Scheme name without `:`.
        scheme: String,
        /// Callback path.
        path: String,
    },
    /// HTTPS universal link.
    UniversalLink {
        /// Registered HTTPS origin.
        origin: Url,
        /// Callback path.
        path: String,
    },
    /// Manually pasted authorization code or redirect URL.
    ManualPaste,
}

/// Host request for one redirect receiver (part 2 §6.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedirectReceiverRequest {
    /// Challenge whose callback is being received.
    pub challenge_id: AuthChallengeId,
    /// Provider-supported strategies in preference order.
    pub preferred: Vec<RedirectStrategy>,
    /// Expected callback path when separately constrained.
    pub expected_path: Option<String>,
    /// Browser page rendered for a valid callback.
    pub success_page: AuthHtmlPage,
    /// Browser page rendered for an invalid callback.
    pub failure_page: AuthHtmlPage,
}

/// A concise description retained by unsupported-strategy errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedirectStrategyDescription {
    /// Required provider strategies in preference order.
    pub required: Vec<RedirectStrategy>,
}

/// Host-delivered redirect callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedirectArrival {
    /// Complete callback URL.
    pub url: Url,
    /// Host-observed arrival time.
    pub received_at: Timestamp,
}

/// Send redirect receiver owned by the host.
pub trait RedirectReceiver: Send + 'static {
    /// Actual redirect URI placed into the provider authorization request.
    fn redirect_uri(&self) -> &Url;

    /// Waits for one callback and consumes the receiver.
    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>>;
}

/// Local-executor counterpart to [`RedirectReceiver`].
pub trait LocalRedirectReceiver: 'static {
    /// Actual redirect URI placed into the provider authorization request.
    fn redirect_uri(&self) -> &Url;

    /// Waits for one callback and consumes the receiver.
    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>>;
}

/// Provider-authentication failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthError {
    /// Host cannot receive any redirect strategy required by the provider.
    UnsupportedRedirectStrategy {
        /// Provider whose flow cannot be hosted.
        provider: ProviderId,
        /// Provider-supported redirect strategies.
        required: RedirectStrategyDescription,
        /// Capabilities reported by the host.
        host_capabilities: AuthHostCapabilities,
    },
    /// OAuth callback state did not match the generated state.
    StateMismatch,
    /// Parent or child cancellation ended the operation.
    Cancelled,
    /// Both raced completion paths failed validation.
    NoValidCompletion {
        /// First path's sanitized error.
        first: Box<AuthError>,
        /// Second path's sanitized error.
        second: Box<AuthError>,
    },
    /// Credential storage failed.
    Store {
        /// Store failure.
        source: StoreError,
    },
    /// Host interaction failed.
    Interaction {
        /// Host failure.
        source: AuthInteractionError,
    },
    /// Provider does not support the requested login operation.
    UnsupportedLogin {
        /// Sanitized explanation.
        message: String,
    },
    /// Provider or flow-specific sanitized failure.
    Other {
        /// Stable error code.
        code: String,
        /// Sanitized explanation.
        message: String,
    },
}

impl AuthError {
    /// Creates a sanitized provider/flow failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Other {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Returns the stable error code.
    pub fn code(&self) -> &str {
        match self {
            Self::UnsupportedRedirectStrategy { .. } => "unsupported_redirect_strategy",
            Self::StateMismatch => "oauth_state_mismatch",
            Self::Cancelled => "cancelled",
            Self::NoValidCompletion { .. } => "no_valid_auth_completion",
            Self::Store { .. } | Self::Interaction { .. } | Self::UnsupportedLogin { .. } => "auth",
            Self::Other { code, .. } => code,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedRedirectStrategy {
                provider, required, ..
            } => write!(
                formatter,
                "provider {provider} requires an unsupported redirect strategy: {:?}",
                required.required
            ),
            Self::StateMismatch => formatter.write_str("OAuth state mismatch"),
            Self::Cancelled => formatter.write_str("authentication cancelled"),
            Self::NoValidCompletion { first, second } => {
                write!(
                    formatter,
                    "no valid authentication completion: {first}; {second}"
                )
            }
            Self::Store { source } => write!(formatter, "credential store failed: {source}"),
            Self::Interaction { source } => fmt::Display::fmt(source, formatter),
            Self::UnsupportedLogin { message } | Self::Other { message, .. } => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for AuthError {}

impl From<StoreError> for AuthError {
    fn from(source: StoreError) -> Self {
        Self::Store { source }
    }
}

impl From<AuthInteractionError> for AuthError {
    fn from(source: AuthInteractionError) -> Self {
        match source {
            AuthInteractionError::Cancelled => Self::Cancelled,
            source => Self::Interaction { source },
        }
    }
}

/// Per-request auth overrides. Explicit values are considered only when the
/// provider supports the corresponding auth method.
#[derive(Clone, Default)]
pub struct AuthResolutionOverrides {
    /// Explicit request API key, which wins over stored and ambient values.
    pub api_key: Option<SecretString>,
    /// Provider-scoped environment values overlaid on the host context.
    pub environment: BTreeMap<String, String>,
    /// Required remaining OAuth validity. Ordinary request auth has a
    /// five-minute default window even when this is absent; catalog refresh
    /// ignores it and refreshes only on actual expiry.
    pub min_oauth_validity: Option<Duration>,
}

impl fmt::Debug for AuthResolutionOverrides {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthResolutionOverrides")
            .field("api_key", &self.api_key)
            .field("environment", &"[REDACTED PROVIDER ENVIRONMENT]")
            .field("min_oauth_validity", &self.min_oauth_validity)
            .finish()
    }
}

/// Request supplied to an API-key method after precedence has selected the
/// stored, explicit, or ambient path.
#[derive(Clone)]
pub struct ApiKeyResolveRequest {
    /// Provider metadata.
    pub provider: ProviderDescriptor,
    /// Selected explicit/stored credential, or `None` for ambient resolution.
    pub credential: Option<ApiKeyCredential>,
    /// Host ambient context.
    pub context: Arc<dyn AuthContext>,
    /// Provider-scoped request environment overrides.
    pub environment: BTreeMap<String, String>,
}

/// Provider-owned API-key resolution and interactive setup.
pub trait ApiKeyAuth: Send + Sync + 'static {
    /// Human-readable login method name.
    fn name(&self) -> &str;

    /// Resolves a selected credential or ambient configuration.
    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>>;

    /// Interactively obtains an API-key credential.
    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        let _ = interaction;
        let _ = cancellation;
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "API-key login is not supported".into(),
            })
        })
    }
}

/// Local-executor request supplied to [`LocalApiKeyAuth`].
#[derive(Clone)]
pub struct LocalApiKeyResolveRequest {
    /// Provider metadata.
    pub provider: ProviderDescriptor,
    /// Selected explicit/stored credential, or `None` for ambient resolution.
    pub credential: Option<ApiKeyCredential>,
    /// Local host ambient context.
    pub context: Rc<dyn LocalAuthContext>,
    /// Provider-scoped request environment overrides.
    pub environment: BTreeMap<String, String>,
}

/// Local-executor counterpart to [`ApiKeyAuth`].
pub trait LocalApiKeyAuth: 'static {
    /// Human-readable login method name.
    fn name(&self) -> &str;

    /// Resolves a selected credential or ambient configuration.
    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>>;

    /// Interactively obtains an API-key credential.
    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        let _ = interaction;
        let _ = cancellation;
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "API-key login is not supported".into(),
            })
        })
    }
}

/// Standard stored-key-then-environment API-key method from pinned Pi.
#[derive(Clone, Debug)]
pub struct EnvironmentApiKeyAuth {
    name: String,
    environment_variables: Vec<String>,
}

impl EnvironmentApiKeyAuth {
    /// Creates an API-key method with ambient variables checked in order.
    pub fn new(
        name: impl Into<String>,
        environment_variables: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            environment_variables: environment_variables.into_iter().map(Into::into).collect(),
        }
    }
}

impl ApiKeyAuth for EnvironmentApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let variables = self.environment_variables.clone();
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            if let Some(credential) = request.credential
                && let Some(key) = credential.key
                && !key.expose_secret().is_empty()
            {
                return Ok(Some(ResolvedAuth {
                    api_key: Some(key),
                    headers: HeaderMap::new(),
                    base_url: None,
                    source: AuthSource::new("stored credential"),
                }));
            }

            for variable in variables {
                let value = if let Some(value) = request
                    .environment
                    .get(&variable)
                    .filter(|value| !value.is_empty())
                {
                    Some(value.clone())
                } else {
                    request
                        .context
                        .env(variable.clone(), cancellation.clone())
                        .await?
                };
                cancellation.check().map_err(|_| AuthError::Cancelled)?;
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    return Ok(Some(ResolvedAuth {
                        api_key: Some(SecretString::new(value)),
                        headers: HeaderMap::new(),
                        base_url: None,
                        source: AuthSource::new(variable),
                    }));
                }
            }
            Ok(None)
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        let message = format!("Enter {}", self.name);
        Box::pin(async move {
            let answer = interaction
                .prompt(
                    AuthPrompt::Secret {
                        message,
                        placeholder: None,
                    },
                    cancellation.clone(),
                )
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let AuthAnswer::Text(key) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "secret prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: Some(SecretString::new(key)),
                environment: BTreeMap::new(),
            })
        })
    }
}

impl LocalApiKeyAuth for EnvironmentApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let variables = self.environment_variables.clone();
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            if let Some(credential) = request.credential
                && let Some(key) = credential.key
                && !key.expose_secret().is_empty()
            {
                return Ok(Some(ResolvedAuth {
                    api_key: Some(key),
                    headers: HeaderMap::new(),
                    base_url: None,
                    source: AuthSource::new("stored credential"),
                }));
            }

            for variable in variables {
                let value = if let Some(value) = request
                    .environment
                    .get(&variable)
                    .filter(|value| !value.is_empty())
                {
                    Some(value.clone())
                } else {
                    request
                        .context
                        .env(variable.clone(), cancellation.clone())
                        .await?
                };
                cancellation.check().map_err(|_| AuthError::Cancelled)?;
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    return Ok(Some(ResolvedAuth {
                        api_key: Some(SecretString::new(value)),
                        headers: HeaderMap::new(),
                        base_url: None,
                        source: AuthSource::new(variable),
                    }));
                }
            }
            Ok(None)
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        let message = format!("Enter {}", self.name);
        Box::pin(async move {
            let answer = interaction
                .prompt(
                    AuthPrompt::Secret {
                        message,
                        placeholder: None,
                    },
                    cancellation.clone(),
                )
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let AuthAnswer::Text(key) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "secret prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: Some(SecretString::new(key)),
                environment: BTreeMap::new(),
            })
        })
    }
}

/// Provider-owned OAuth login, refresh, and request-auth derivation.
pub trait OAuthAuth: Send + Sync + 'static {
    /// Human-readable login method name.
    fn name(&self) -> &str;

    /// Runs the provider login flow.
    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>>;

    /// Exchanges a refresh token for a rotated credential.
    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>>;

    /// Derives request auth without network side effects.
    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>>;
}

/// Local-executor counterpart to [`OAuthAuth`].
pub trait LocalOAuthAuth: 'static {
    /// Human-readable login method name.
    fn name(&self) -> &str;

    /// Runs the provider login flow.
    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>>;

    /// Exchanges a refresh token for a rotated credential.
    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>>;

    /// Derives request auth without network side effects.
    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>>;
}

/// Time source used by OAuth expiry checks.
pub trait AuthClock: Send + Sync + 'static {
    /// Returns current Unix time in milliseconds.
    fn now(&self) -> Timestamp;
}

/// Local-executor counterpart to [`AuthClock`].
pub trait LocalAuthClock: 'static {
    /// Returns current Unix time in milliseconds.
    fn now(&self) -> Timestamp;
}

/// System clock used by native/default auth resolution.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemAuthClock;

impl AuthClock for SystemAuthClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
    }
}

impl LocalAuthClock for SystemAuthClock {
    fn now(&self) -> Timestamp {
        AuthClock::now(self)
    }
}

/// Standard provider resolver implementing pinned Pi precedence and OAuth
/// double-checked refresh under a credential lease.
pub struct ProviderAuthResolver {
    api_key: Option<Arc<dyn ApiKeyAuth>>,
    oauth: Option<Arc<dyn OAuthAuth>>,
    clock: Arc<dyn AuthClock>,
}

impl fmt::Debug for ProviderAuthResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthResolver")
            .field(
                "api_key",
                &self.api_key.as_ref().map(|method| method.name()),
            )
            .field("oauth", &self.oauth.as_ref().map(|method| method.name()))
            .finish_non_exhaustive()
    }
}

impl ProviderAuthResolver {
    /// Creates a resolver from optional API-key and OAuth methods.
    pub fn new(api_key: Option<Arc<dyn ApiKeyAuth>>, oauth: Option<Arc<dyn OAuthAuth>>) -> Self {
        Self {
            api_key,
            oauth,
            clock: Arc::new(SystemAuthClock),
        }
    }

    /// Injects a deterministic expiry clock.
    pub fn with_clock(mut self, clock: Arc<dyn AuthClock>) -> Self {
        self.clock = clock;
        self
    }

    async fn resolve_oauth(
        &self,
        request: &crate::ResolveAuthRequest,
        stored: OAuthCredential,
        cancellation: CancellationToken,
    ) -> Result<Option<ResolvedAuth>, AuthError> {
        let Some(oauth) = &self.oauth else {
            return Ok(None);
        };
        const DEFAULT_VALIDITY: Duration = Duration::from_secs(5 * 60);
        let (requested, minimum) = match request.purpose {
            AuthResolutionPurpose::Request => {
                let requested = request.overrides.min_oauth_validity;
                (
                    requested,
                    requested.unwrap_or_default().max(DEFAULT_VALIDITY),
                )
            }
            AuthResolutionPurpose::CatalogRefresh => (None, Duration::ZERO),
        };
        let expires_soon = |credential: &OAuthCredential| {
            let minimum_millis = i64::try_from(minimum.as_millis()).unwrap_or(i64::MAX);
            self.clock
                .now()
                .unix_millis()
                .saturating_add(minimum_millis)
                >= credential.expires_at.unix_millis()
        };

        let mut credential = stored;
        if expires_soon(&credential) {
            let mut lease = request
                .credential_store
                .acquire_lease(request.provider.id.clone(), cancellation.clone())
                .await?;
            let current = match lease.current() {
                Some(Credential::OAuth(current)) => current.clone(),
                _ => return Ok(None),
            };
            if expires_soon(&current) {
                let refresh = match request.purpose {
                    AuthResolutionPurpose::Request => {
                        refresh_oauth_with_timeout(oauth.as_ref(), current, cancellation.clone())
                            .await
                    }
                    AuthResolutionPurpose::CatalogRefresh => {
                        oauth.refresh(current, cancellation.clone()).await
                    }
                };
                let refreshed = match refresh {
                    Ok(credential) => credential,
                    Err(AuthError::Cancelled) => return Err(AuthError::Cancelled),
                    Err(error) => {
                        return Err(AuthError::new(
                            "oauth",
                            format!("OAuth refresh failed for {}: {error}", request.provider.id),
                        ));
                    }
                };
                credential = refreshed.clone();
                lease.replace(Some(Credential::OAuth(refreshed)));
                lease.commit().await?;
                if requested.is_some() && expires_soon(&credential) {
                    return Err(AuthError::new(
                        "oauth",
                        format!(
                            "OAuth refresh returned a token that expires too soon for {}",
                            request.provider.id
                        ),
                    ));
                }
            } else {
                credential = current;
            }
        }

        let mut resolved = oauth.to_auth(&credential).await.map_err(|error| {
            AuthError::new(
                "oauth",
                format!(
                    "OAuth auth derivation failed for {}: {error}",
                    request.provider.id
                ),
            )
        })?;
        resolved.source = AuthSource::new("OAuth");
        Ok(Some(resolved))
    }
}

async fn refresh_oauth_with_timeout(
    oauth: &dyn OAuthAuth,
    credential: OAuthCredential,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

    let refresh_cancellation = cancellation.child();
    let refresh = Box::pin(oauth.refresh(credential, refresh_cancellation.clone()));
    let timeout = Box::pin(futures_timer::Delay::new(REFRESH_TIMEOUT));
    let cancelled = Box::pin(cancellation.cancelled());
    let stopped = Box::pin(select(timeout, cancelled));

    match select(refresh, stopped).await {
        Either::Left((result, _)) => result,
        Either::Right((Either::Left(((), _)), _)) => {
            refresh_cancellation.cancel();
            Err(AuthError::new(
                "oauth_refresh_timeout",
                "OAuth refresh timed out after 15 seconds",
            ))
        }
        Either::Right((Either::Right(((), _)), _)) => {
            refresh_cancellation.cancel();
            Err(AuthError::Cancelled)
        }
    }
}

impl crate::AuthResolver for ProviderAuthResolver {
    fn resolve(
        &self,
        request: crate::ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;

            if let Some(explicit) = request.overrides.api_key.clone()
                && let Some(api_key) = &self.api_key
            {
                return api_key
                    .resolve(
                        ApiKeyResolveRequest {
                            provider: request.provider.clone(),
                            credential: Some(ApiKeyCredential {
                                key: Some(explicit),
                                environment: request.overrides.environment.clone(),
                            }),
                            context: Arc::clone(&request.auth_context),
                            environment: request.overrides.environment.clone(),
                        },
                        cancellation,
                    )
                    .await;
            }

            let stored = request
                .credential_store
                .read(request.provider.id.clone(), cancellation.clone())
                .await?;
            if let Some(stored) = stored {
                return match stored {
                    Credential::OAuth(credential) => {
                        self.resolve_oauth(&request, credential, cancellation).await
                    }
                    Credential::ApiKey(mut credential) => {
                        let Some(api_key) = &self.api_key else {
                            return Ok(None);
                        };
                        credential
                            .environment
                            .extend(request.overrides.environment.clone());
                        api_key
                            .resolve(
                                ApiKeyResolveRequest {
                                    provider: request.provider.clone(),
                                    credential: Some(credential),
                                    context: Arc::clone(&request.auth_context),
                                    environment: request.overrides.environment.clone(),
                                },
                                cancellation,
                            )
                            .await
                    }
                };
            }

            let Some(api_key) = &self.api_key else {
                return Ok(None);
            };
            api_key
                .resolve(
                    ApiKeyResolveRequest {
                        provider: request.provider,
                        credential: None,
                        context: request.auth_context,
                        environment: request.overrides.environment,
                    },
                    cancellation,
                )
                .await
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Credential, AuthError>> {
        Box::pin(async move {
            let choice = match (&self.api_key, &self.oauth) {
                (Some(_), Some(_)) => {
                    let answer = interaction
                        .prompt(
                            AuthPrompt::Select {
                                message: "Select authentication method:".into(),
                                options: vec![
                                    AuthSelectOption {
                                        id: "oauth".into(),
                                        label: self.oauth.as_ref().expect("present").name().into(),
                                        description: None,
                                    },
                                    AuthSelectOption {
                                        id: "api_key".into(),
                                        label: self
                                            .api_key
                                            .as_ref()
                                            .expect("present")
                                            .name()
                                            .into(),
                                        description: None,
                                    },
                                ],
                            },
                            cancellation.clone(),
                        )
                        .await?;
                    let AuthAnswer::Selected(choice) = answer else {
                        return Err(AuthError::new(
                            "invalid_auth_answer",
                            "auth selection returned a non-selection answer",
                        ));
                    };
                    choice
                }
                (None, Some(_)) => "oauth".into(),
                (Some(_), None) => "api_key".into(),
                (None, None) => {
                    return Err(AuthError::UnsupportedLogin {
                        message: "provider has no interactive authentication method".into(),
                    });
                }
            };

            match choice.as_str() {
                "oauth" => Ok(Credential::OAuth(
                    self.oauth
                        .as_ref()
                        .ok_or_else(|| AuthError::UnsupportedLogin {
                            message: "OAuth login is not supported".into(),
                        })?
                        .login(interaction, cancellation)
                        .await?,
                )),
                "api_key" => Ok(Credential::ApiKey(
                    self.api_key
                        .as_ref()
                        .ok_or_else(|| AuthError::UnsupportedLogin {
                            message: "API-key login is not supported".into(),
                        })?
                        .login(interaction, cancellation)
                        .await?,
                )),
                other => Err(AuthError::new(
                    "unknown_auth_method",
                    format!("unknown authentication method: {other}"),
                )),
            }
        })
    }
}

/// Local-executor provider resolver with the same precedence and lease
/// semantics as [`ProviderAuthResolver`].
pub struct LocalProviderAuthResolver {
    api_key: Option<Rc<dyn LocalApiKeyAuth>>,
    oauth: Option<Rc<dyn LocalOAuthAuth>>,
    clock: Rc<dyn LocalAuthClock>,
}

impl fmt::Debug for LocalProviderAuthResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalProviderAuthResolver")
            .field(
                "api_key",
                &self.api_key.as_ref().map(|method| method.name()),
            )
            .field("oauth", &self.oauth.as_ref().map(|method| method.name()))
            .finish_non_exhaustive()
    }
}

impl LocalProviderAuthResolver {
    /// Creates a local resolver from optional API-key and OAuth methods.
    pub fn new(
        api_key: Option<Rc<dyn LocalApiKeyAuth>>,
        oauth: Option<Rc<dyn LocalOAuthAuth>>,
    ) -> Self {
        Self {
            api_key,
            oauth,
            clock: Rc::new(SystemAuthClock),
        }
    }

    /// Injects a deterministic local expiry clock.
    pub fn with_clock(mut self, clock: Rc<dyn LocalAuthClock>) -> Self {
        self.clock = clock;
        self
    }

    async fn resolve_oauth(
        &self,
        request: &crate::LocalResolveAuthRequest,
        stored: OAuthCredential,
        cancellation: CancellationToken,
    ) -> Result<Option<ResolvedAuth>, AuthError> {
        let Some(oauth) = &self.oauth else {
            return Ok(None);
        };
        const DEFAULT_VALIDITY: Duration = Duration::from_secs(5 * 60);
        let (requested, minimum) = match request.purpose {
            AuthResolutionPurpose::Request => {
                let requested = request.overrides.min_oauth_validity;
                (
                    requested,
                    requested.unwrap_or_default().max(DEFAULT_VALIDITY),
                )
            }
            AuthResolutionPurpose::CatalogRefresh => (None, Duration::ZERO),
        };
        let expires_soon = |credential: &OAuthCredential| {
            let minimum_millis = i64::try_from(minimum.as_millis()).unwrap_or(i64::MAX);
            self.clock
                .now()
                .unix_millis()
                .saturating_add(minimum_millis)
                >= credential.expires_at.unix_millis()
        };

        let mut credential = stored;
        if expires_soon(&credential) {
            let mut lease = request
                .credential_store
                .acquire_lease(request.provider.id.clone(), cancellation.clone())
                .await?;
            let current = match lease.current() {
                Some(Credential::OAuth(current)) => current.clone(),
                _ => return Ok(None),
            };
            if expires_soon(&current) {
                let refresh = match request.purpose {
                    AuthResolutionPurpose::Request => {
                        refresh_local_oauth_with_timeout(
                            oauth.as_ref(),
                            current,
                            cancellation.clone(),
                        )
                        .await
                    }
                    AuthResolutionPurpose::CatalogRefresh => {
                        oauth.refresh(current, cancellation.clone()).await
                    }
                };
                let refreshed = match refresh {
                    Ok(credential) => credential,
                    Err(AuthError::Cancelled) => return Err(AuthError::Cancelled),
                    Err(error) => {
                        return Err(AuthError::new(
                            "oauth",
                            format!("OAuth refresh failed for {}: {error}", request.provider.id),
                        ));
                    }
                };
                credential = refreshed.clone();
                lease.replace(Some(Credential::OAuth(refreshed)));
                lease.commit().await?;
                if requested.is_some() && expires_soon(&credential) {
                    return Err(AuthError::new(
                        "oauth",
                        format!(
                            "OAuth refresh returned a token that expires too soon for {}",
                            request.provider.id
                        ),
                    ));
                }
            } else {
                credential = current;
            }
        }

        let mut resolved = oauth.to_auth(&credential).await.map_err(|error| {
            AuthError::new(
                "oauth",
                format!(
                    "OAuth auth derivation failed for {}: {error}",
                    request.provider.id
                ),
            )
        })?;
        resolved.source = AuthSource::new("OAuth");
        Ok(Some(resolved))
    }
}

async fn refresh_local_oauth_with_timeout(
    oauth: &dyn LocalOAuthAuth,
    credential: OAuthCredential,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

    let refresh_cancellation = cancellation.child();
    let refresh = Box::pin(oauth.refresh(credential, refresh_cancellation.clone()));
    let timeout = Box::pin(futures_timer::Delay::new(REFRESH_TIMEOUT));
    let cancelled = Box::pin(cancellation.cancelled());
    let stopped = Box::pin(select(timeout, cancelled));

    match select(refresh, stopped).await {
        Either::Left((result, _)) => result,
        Either::Right((Either::Left(((), _)), _)) => {
            refresh_cancellation.cancel();
            Err(AuthError::new(
                "oauth_refresh_timeout",
                "OAuth refresh timed out after 15 seconds",
            ))
        }
        Either::Right((Either::Right(((), _)), _)) => {
            refresh_cancellation.cancel();
            Err(AuthError::Cancelled)
        }
    }
}

impl crate::LocalAuthResolver for LocalProviderAuthResolver {
    fn resolve(
        &self,
        request: crate::LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;

            if let Some(explicit) = request.overrides.api_key.clone()
                && let Some(api_key) = &self.api_key
            {
                return api_key
                    .resolve(
                        LocalApiKeyResolveRequest {
                            provider: request.provider.clone(),
                            credential: Some(ApiKeyCredential {
                                key: Some(explicit),
                                environment: request.overrides.environment.clone(),
                            }),
                            context: Rc::clone(&request.auth_context),
                            environment: request.overrides.environment.clone(),
                        },
                        cancellation,
                    )
                    .await;
            }

            let stored = request
                .credential_store
                .read(request.provider.id.clone(), cancellation.clone())
                .await?;
            if let Some(stored) = stored {
                return match stored {
                    Credential::OAuth(credential) => {
                        self.resolve_oauth(&request, credential, cancellation).await
                    }
                    Credential::ApiKey(mut credential) => {
                        let Some(api_key) = &self.api_key else {
                            return Ok(None);
                        };
                        credential
                            .environment
                            .extend(request.overrides.environment.clone());
                        api_key
                            .resolve(
                                LocalApiKeyResolveRequest {
                                    provider: request.provider.clone(),
                                    credential: Some(credential),
                                    context: Rc::clone(&request.auth_context),
                                    environment: request.overrides.environment.clone(),
                                },
                                cancellation,
                            )
                            .await
                    }
                };
            }

            let Some(api_key) = &self.api_key else {
                return Ok(None);
            };
            api_key
                .resolve(
                    LocalApiKeyResolveRequest {
                        provider: request.provider,
                        credential: None,
                        context: request.auth_context,
                        environment: request.overrides.environment,
                    },
                    cancellation,
                )
                .await
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Credential, AuthError>> {
        Box::pin(async move {
            let choice = match (&self.api_key, &self.oauth) {
                (Some(api_key), Some(oauth)) => {
                    let answer = interaction
                        .prompt(
                            AuthPrompt::Select {
                                message: "Select authentication method:".into(),
                                options: vec![
                                    AuthSelectOption {
                                        id: "oauth".into(),
                                        label: oauth.name().into(),
                                        description: None,
                                    },
                                    AuthSelectOption {
                                        id: "api_key".into(),
                                        label: api_key.name().into(),
                                        description: None,
                                    },
                                ],
                            },
                            cancellation.clone(),
                        )
                        .await?;
                    let AuthAnswer::Selected(choice) = answer else {
                        return Err(AuthError::new(
                            "invalid_auth_answer",
                            "auth selection returned a non-selection answer",
                        ));
                    };
                    choice
                }
                (None, Some(_)) => "oauth".into(),
                (Some(_), None) => "api_key".into(),
                (None, None) => {
                    return Err(AuthError::UnsupportedLogin {
                        message: "provider has no interactive authentication method".into(),
                    });
                }
            };

            match choice.as_str() {
                "oauth" => Ok(Credential::OAuth(
                    self.oauth
                        .as_ref()
                        .ok_or_else(|| AuthError::UnsupportedLogin {
                            message: "OAuth login is not supported".into(),
                        })?
                        .login(interaction, cancellation)
                        .await?,
                )),
                "api_key" => Ok(Credential::ApiKey(
                    self.api_key
                        .as_ref()
                        .ok_or_else(|| AuthError::UnsupportedLogin {
                            message: "API-key login is not supported".into(),
                        })?
                        .login(interaction, cancellation)
                        .await?,
                )),
                other => Err(AuthError::new(
                    "unknown_auth_method",
                    format!("unknown authentication method: {other}"),
                )),
            }
        })
    }
}

/// Returns whether a host capability set can implement one strategy.
pub fn redirect_strategy_supported(
    strategy: &RedirectStrategy,
    capabilities: &AuthHostCapabilities,
) -> bool {
    match strategy {
        RedirectStrategy::FixedLoopback { .. } | RedirectStrategy::EphemeralLoopback { .. } => {
            capabilities.loopback_http
        }
        RedirectStrategy::CustomScheme { .. } => capabilities.custom_url_scheme,
        RedirectStrategy::UniversalLink { .. } => capabilities.universal_links,
        RedirectStrategy::ManualPaste => capabilities.manual_paste,
    }
}

/// Creates a receiver only when the host advertises at least one provider
/// strategy, otherwise returns the architecture-mandated explicit error.
pub async fn create_supported_redirect_receiver(
    provider: ProviderId,
    interaction: Arc<dyn AuthInteraction>,
    request: RedirectReceiverRequest,
    cancellation: CancellationToken,
) -> Result<Box<dyn RedirectReceiver>, AuthError> {
    let capabilities = interaction.capabilities();
    if !request
        .preferred
        .iter()
        .any(|strategy| redirect_strategy_supported(strategy, &capabilities))
    {
        return Err(AuthError::UnsupportedRedirectStrategy {
            provider,
            required: RedirectStrategyDescription {
                required: request.preferred,
            },
            host_capabilities: capabilities,
        });
    }
    interaction
        .create_redirect_receiver(request, cancellation)
        .await
        .map_err(AuthError::from)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
