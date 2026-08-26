//! Provenance-preserving model catalogs from Architecture v2 part 1 §3.7
//! and part 2 §5.3–§5.7.

use crate::{
    ApiId, ApiModelConfig, CancellationToken, ExtensionMap, HeaderMapSpec, LocalBoxFuture,
    ModelDescriptor, ModelLimits, ModelPricing, ModelRef, ProviderId, ResolvedAuth, SendBoxFuture,
    Timestamp,
};
use arc_swap::ArcSwap;
use futures_util::future::{Either, select};
use futures_util::lock::Mutex as AsyncMutex;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use url::Url;

/// Current native schema for persisted catalog and override values.
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

/// Immutable complete model snapshot visible to synchronous readers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    /// Persistence schema version.
    pub schema_version: u32,
    /// Complete effective model list in stable catalog order.
    pub models: Arc<[ModelDescriptor]>,
    /// Time at which the provider-owned dynamic source was checked.
    pub checked_at: Timestamp,
    /// Provider-owned revision label, when supplied.
    pub revision: Option<String>,
    /// Provider-owned opaque HTTP or protocol validator.
    pub etag: Option<String>,
    /// Provider-owned namespaced metadata that survives persistence.
    pub source_metadata: ExtensionMap,
}

impl CatalogSnapshot {
    /// Creates a baseline snapshot with no dynamic-source metadata.
    pub fn baseline(models: impl Into<Vec<ModelDescriptor>>) -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            models: Arc::from(models.into()),
            checked_at: Timestamp::default(),
            revision: None,
            etag: None,
            source_metadata: ExtensionMap::new(),
        }
    }
}

/// Validated provider-owned network candidate before durable publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCandidate {
    /// Complete provider-owned dynamic model list.
    pub models: Vec<ModelDescriptor>,
    /// Time at which the source was checked.
    pub checked_at: Timestamp,
    /// Provider-owned revision label, when supplied.
    pub revision: Option<String>,
    /// Provider-owned opaque HTTP or protocol validator.
    pub etag: Option<String>,
    /// Provider-owned namespaced metadata.
    pub source_metadata: ExtensionMap,
}

impl CatalogCandidate {
    /// Converts this candidate to the durable provider-owned representation.
    pub fn to_persisted(&self) -> PersistedCatalogSnapshot {
        PersistedCatalogSnapshot {
            schema_version: CATALOG_SCHEMA_VERSION,
            models: self.models.clone(),
            checked_at: self.checked_at,
            revision: self.revision.clone(),
            etag: self.etag.clone(),
            source_metadata: self.source_metadata.clone(),
        }
    }

    fn into_snapshot(self) -> CatalogSnapshot {
        CatalogSnapshot {
            schema_version: CATALOG_SCHEMA_VERSION,
            models: Arc::from(self.models),
            checked_at: self.checked_at,
            revision: self.revision,
            etag: self.etag,
            source_metadata: self.source_metadata,
        }
    }
}

/// Durable provider-owned dynamic snapshot stored independently of host policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedCatalogSnapshot {
    /// Persistence schema version.
    pub schema_version: u32,
    /// Complete raw provider-owned model list.
    pub models: Vec<ModelDescriptor>,
    /// Time at which the source was checked.
    pub checked_at: Timestamp,
    /// Provider-owned revision label, when supplied.
    pub revision: Option<String>,
    /// Provider-owned opaque HTTP or protocol validator.
    pub etag: Option<String>,
    /// Provider-owned namespaced metadata.
    pub source_metadata: ExtensionMap,
}

impl PersistedCatalogSnapshot {
    /// Converts a supported persisted value back to a validation candidate.
    pub fn to_candidate(&self) -> Result<CatalogCandidate, CatalogError> {
        if self.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::validation(format!(
                "unsupported catalog schema version: {}",
                self.schema_version
            )));
        }
        Ok(CatalogCandidate {
            models: self.models.clone(),
            checked_at: self.checked_at,
            revision: self.revision.clone(),
            etag: self.etag.clone(),
            source_metadata: self.source_metadata.clone(),
        })
    }
}

/// All provenance layers for one provider catalog.
#[derive(Clone, Debug)]
pub struct ProviderCatalogLayers {
    /// Generated or provider-factory baseline.
    pub baseline: Arc<[ModelDescriptor]>,
    /// Last durable provider-owned dynamic snapshot restored from storage.
    pub restored_dynamic: Option<Arc<CatalogSnapshot>>,
    /// Most recent validated provider-owned network snapshot.
    pub network_dynamic: Option<Arc<CatalogSnapshot>>,
    /// Host-managed persistent policy overrides.
    pub host_overrides: Arc<[ModelOverride]>,
    /// Process-local temporary registrations and overrides.
    pub runtime_overrides: Arc<[ModelOverride]>,
}

/// One versioned host or runtime model override.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelOverride {
    /// Persistence schema version.
    pub schema_version: u32,
    /// Provider/model identity affected by this override.
    pub model_ref: ModelRef,
    /// Override operation.
    pub action: ModelOverrideAction,
}

impl ModelOverride {
    /// Creates a full model upsert override.
    pub fn add(descriptor: ModelDescriptor) -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            model_ref: descriptor.common.model_ref.clone(),
            action: ModelOverrideAction::Add { descriptor },
        }
    }

    /// Creates a hide override.
    pub fn hide(model_ref: ModelRef) -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            model_ref,
            action: ModelOverrideAction::Hide,
        }
    }

    /// Creates a typed patch override.
    pub fn patch(model_ref: ModelRef, patch: ModelOverridePatch) -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            model_ref,
            action: ModelOverrideAction::Patch { patch },
        }
    }
}

/// Operation performed by a model override.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelOverrideAction {
    /// Add or fully replace a model descriptor.
    Add {
        /// Complete replacement descriptor.
        descriptor: ModelDescriptor,
    },
    /// Hide a model from the effective catalog without deleting source data.
    Hide,
    /// Patch typed fields on an existing model.
    Patch {
        /// Typed patch value.
        patch: ModelOverridePatch,
    },
}

/// Typed patch for fields an override is allowed to change.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelOverridePatch {
    /// Replacement display name.
    pub display_name: Option<String>,
    /// Replacement base URL.
    pub base_url: Option<Url>,
    /// Replacement common token limits.
    pub limits: Option<ModelLimits>,
    /// Replacement integer pricing.
    pub pricing: Option<ModelPricing>,
    /// Replacement reasoning capability flag.
    pub reasoning: Option<bool>,
    /// Case-insensitive per-model header overlay; `None` deletes a name.
    pub headers: HeaderMapSpec,
    /// Expected resulting API family when changing typed configuration.
    pub api: Option<ApiId>,
    /// Complete typed API-family configuration replacement.
    pub api_config: Option<ApiModelConfig>,
    /// Namespaced extension overlay.
    pub extensions: ExtensionMap,
}

/// Provider and prior-state inputs supplied to a dynamic catalog source.
#[derive(Clone, Debug)]
pub struct CatalogFetchContext {
    /// Provider being refreshed.
    pub provider: ProviderId,
    /// Raw provider-owned snapshot restored before authentication.
    pub stored: Option<Arc<PersistedCatalogSnapshot>>,
    /// Effective provider-scoped authentication resolved after restoration.
    pub auth: ResolvedAuth,
    /// Whether source freshness checks should be bypassed.
    pub force: bool,
}

/// Send-capable provider-owned model catalog source.
pub trait ModelCatalogSource: Send + Sync + 'static {
    /// Returns the provider-factory baseline.
    fn baseline(&self) -> Arc<[ModelDescriptor]>;

    /// Fetches one complete provider-owned dynamic candidate.
    fn fetch(
        &self,
        context: CatalogFetchContext,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CatalogCandidate, CatalogError>>;
}

/// Single-threaded provider-owned model catalog source.
pub trait LocalModelCatalogSource: 'static {
    /// Returns the provider-factory baseline.
    fn baseline(&self) -> Rc<[ModelDescriptor]>;

    /// Fetches one complete provider-owned dynamic candidate.
    fn fetch(
        &self,
        context: CatalogFetchContext,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CatalogCandidate, CatalogError>>;
}

/// Durable provider-originated model snapshot store.
pub trait ModelsStore: Send + Sync + 'static {
    /// Reads one raw provider-owned snapshot.
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>>;

    /// Writes one raw provider-owned snapshot.
    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>>;

    /// Deletes one raw provider-owned snapshot.
    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>>;
}

/// Single-threaded durable provider-originated model snapshot store.
pub trait LocalModelsStore: 'static {
    /// Reads one raw provider-owned snapshot.
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>>;

    /// Writes one raw provider-owned snapshot.
    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>>;

    /// Deletes one raw provider-owned snapshot.
    fn delete(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>>;
}

/// Synchronous host-policy model override store.
pub trait ModelOverrideStore: Send + Sync + 'static {
    /// Returns the last valid immutable override snapshot for a provider.
    fn snapshot(&self, provider: &ProviderId) -> Result<Arc<[ModelOverride]>, OverrideError>;
}

/// Single-threaded synchronous host-policy model override store.
pub trait LocalModelOverrideStore: 'static {
    /// Returns the last valid immutable override snapshot for a provider.
    fn snapshot(&self, provider: &ProviderId) -> Result<Rc<[ModelOverride]>, OverrideError>;
}

/// Thread-safe in-memory implementation of [`ModelsStore`].
#[derive(Debug, Default)]
pub struct InMemoryModelsStore {
    entries: RwLock<BTreeMap<ProviderId, PersistedCatalogSnapshot>>,
}

impl ModelsStore for InMemoryModelsStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
            Ok(read_unpoisoned(&self.entries).get(&provider).cloned())
        })
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), StoreError>> {
        let provider = provider.clone();
        let snapshot = snapshot.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
            write_unpoisoned(&self.entries).insert(provider, snapshot);
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
            cancellation.check().map_err(StoreError::cancelled)?;
            write_unpoisoned(&self.entries).remove(&provider);
            Ok(())
        })
    }
}

impl LocalModelsStore for InMemoryModelsStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
            Ok(read_unpoisoned(&self.entries).get(&provider).cloned())
        })
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>> {
        let provider = provider.clone();
        let snapshot = snapshot.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
            write_unpoisoned(&self.entries).insert(provider, snapshot);
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
            cancellation.check().map_err(StoreError::cancelled)?;
            write_unpoisoned(&self.entries).remove(&provider);
            Ok(())
        })
    }
}

/// Thread-safe in-memory implementation of [`ModelOverrideStore`].
#[derive(Debug, Default)]
pub struct InMemoryModelOverrideStore {
    entries: RwLock<BTreeMap<ProviderId, Arc<[ModelOverride]>>>,
}

impl InMemoryModelOverrideStore {
    /// Replaces a provider's overrides only after basic schema and identity validation.
    pub fn replace(
        &self,
        provider: ProviderId,
        overrides: Vec<ModelOverride>,
    ) -> Result<(), OverrideError> {
        validate_override_snapshot(&provider, &overrides)?;
        write_unpoisoned(&self.entries).insert(provider, Arc::from(overrides));
        Ok(())
    }

    /// Removes a provider's host overrides.
    pub fn remove(&self, provider: &ProviderId) {
        write_unpoisoned(&self.entries).remove(provider);
    }
}

impl ModelOverrideStore for InMemoryModelOverrideStore {
    fn snapshot(&self, provider: &ProviderId) -> Result<Arc<[ModelOverride]>, OverrideError> {
        Ok(read_unpoisoned(&self.entries)
            .get(provider)
            .cloned()
            .unwrap_or_else(|| Arc::from(Vec::new())))
    }
}

impl LocalModelOverrideStore for InMemoryModelOverrideStore {
    fn snapshot(&self, provider: &ProviderId) -> Result<Rc<[ModelOverride]>, OverrideError> {
        let values = read_unpoisoned(&self.entries)
            .get(provider)
            .map(|overrides| overrides.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(Rc::from(values))
    }
}

/// Monotonic identity of one provider refresh operation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RefreshGeneration(pub u64);

/// Provider-ID-scoped coordination shared by every registration generation.
///
/// This is deliberately owned by the Models registry slot rather than by one
/// registration. Replacing a provider must supersede its old work before the
/// replacement becomes visible, and old/new registrations must serialize
/// durable publications through the same mutex.
pub(crate) struct ProviderRefreshCoordination {
    generation: AtomicU64,
    active_refresh: Mutex<Option<(RefreshGeneration, CancellationToken)>>,
    publication: AsyncMutex<()>,
}

impl ProviderRefreshCoordination {
    pub(crate) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active_refresh: Mutex::new(None),
            publication: AsyncMutex::new(()),
        }
    }

    fn begin_refresh(&self, parent: &CancellationToken) -> (RefreshGeneration, CancellationToken) {
        let mut active = lock_unpoisoned(&self.active_refresh);
        let generation = RefreshGeneration(self.generation.fetch_add(1, Ordering::AcqRel) + 1);
        let cancellation = parent.child();
        let previous = active.replace((generation, cancellation.clone()));
        if let Some((_, previous)) = previous {
            previous.cancel();
        }
        (generation, cancellation)
    }

    fn verify_generation(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
    ) -> Result<(), CatalogError> {
        if self.generation.load(Ordering::Acquire) != generation.0 {
            return Err(CatalogError::superseded());
        }
        cancellation.check().map_err(|_| CatalogError::cancelled())
    }

    fn verify_and<T>(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        operation: impl FnOnce() -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        let _active = lock_unpoisoned(&self.active_refresh);
        self.verify_generation(generation, cancellation)?;
        operation()
    }

    fn finish_refresh(&self, generation: RefreshGeneration) {
        let mut active = lock_unpoisoned(&self.active_refresh);
        if active.as_ref().map(|(current, _)| *current) == Some(generation) {
            active.take();
        }
    }

    pub(crate) fn supersede_refresh(&self) {
        let mut active = lock_unpoisoned(&self.active_refresh);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some((_, cancellation)) = active.take() {
            cancellation.cancel();
        }
    }
}

/// Atomically published, provenance-preserving state for one provider.
pub struct ProviderCatalogState {
    provider_id: ProviderId,
    allowed_apis: Arc<[ApiId]>,
    layers: RwLock<ProviderCatalogLayers>,
    published: ArcSwap<CatalogSnapshot>,
    host_override_epoch: AtomicU64,
    coordination: RwLock<Arc<ProviderRefreshCoordination>>,
}

/// Managed [`crate::ModelCatalog`] adapter coupling atomic state to an
/// optional dynamic source without widening [`crate::ProviderRegistration`].
pub struct ManagedModelCatalog {
    state: Arc<ProviderCatalogState>,
    source: Option<Arc<dyn ModelCatalogSource>>,
}

impl fmt::Debug for ManagedModelCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedModelCatalog")
            .field("state", &self.state)
            .field("refreshable", &self.source.is_some())
            .finish()
    }
}

impl ManagedModelCatalog {
    pub(crate) fn new(
        state: Arc<ProviderCatalogState>,
        source: Option<Arc<dyn ModelCatalogSource>>,
    ) -> Self {
        Self { state, source }
    }
}

impl fmt::Debug for ProviderCatalogState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let coordination = self.coordination();
        formatter
            .debug_struct("ProviderCatalogState")
            .field("provider_id", &self.provider_id)
            .field(
                "generation",
                &coordination.generation.load(Ordering::Acquire),
            )
            .field("model_count", &self.published.load().models.len())
            .finish_non_exhaustive()
    }
}

impl ProviderCatalogState {
    /// Creates managed catalog state from one complete provider baseline.
    pub fn new(
        provider_id: ProviderId,
        baseline: Arc<[ModelDescriptor]>,
        allowed_apis: Arc<[ApiId]>,
    ) -> Result<Self, CatalogError> {
        validate_models(&provider_id, &allowed_apis, &baseline)?;
        let initial = CatalogSnapshot::baseline(baseline.iter().cloned().collect::<Vec<_>>());
        Ok(Self {
            provider_id,
            allowed_apis,
            layers: RwLock::new(ProviderCatalogLayers {
                baseline,
                restored_dynamic: None,
                network_dynamic: None,
                host_overrides: Arc::from(Vec::new()),
                runtime_overrides: Arc::from(Vec::new()),
            }),
            published: ArcSwap::from_pointee(initial),
            host_override_epoch: AtomicU64::new(0),
            coordination: RwLock::new(Arc::new(ProviderRefreshCoordination::new())),
        })
    }

    /// Returns this state's provider identity.
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    /// Atomically loads the last complete published snapshot.
    pub fn published_snapshot(&self) -> Arc<CatalogSnapshot> {
        self.published.load_full()
    }

    /// Returns a provenance-layer snapshot for diagnostics and policy updates.
    pub fn layers(&self) -> ProviderCatalogLayers {
        read_unpoisoned(&self.layers).clone()
    }

    pub(crate) fn bind_coordination(&self, coordination: Arc<ProviderRefreshCoordination>) {
        *write_unpoisoned(&self.coordination) = coordination;
    }

    pub(crate) fn coordination(&self) -> Arc<ProviderRefreshCoordination> {
        Arc::clone(&read_unpoisoned(&self.coordination))
    }

    pub(crate) fn begin_refresh(
        &self,
        parent: &CancellationToken,
    ) -> (RefreshGeneration, CancellationToken) {
        self.coordination().begin_refresh(parent)
    }

    pub(crate) fn verify_generation(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_generation(generation, cancellation)
    }

    pub(crate) fn finish_refresh(&self, generation: RefreshGeneration) {
        self.coordination().finish_refresh(generation);
    }

    pub(crate) fn validate_candidate(
        &self,
        candidate: CatalogCandidate,
    ) -> Result<CatalogSnapshot, CatalogError> {
        validate_models(&self.provider_id, &self.allowed_apis, &candidate.models)?;
        Ok(candidate.into_snapshot())
    }

    pub(crate) fn publish_restored(
        &self,
        restored: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = write_unpoisoned(&self.layers);
        let mut next = layers.clone();
        next.restored_dynamic = Some(restored);
        let host_overrides_are_current =
            self.host_override_epoch.load(Ordering::Acquire) == expected_host_override_epoch;
        if host_overrides_are_current {
            next.host_overrides = host_overrides;
        }
        let effective = self.compose_layers(&next)?;
        *layers = next;
        if host_overrides_are_current {
            self.host_override_epoch.fetch_add(1, Ordering::Release);
        }
        self.published.store(Arc::new(effective));
        Ok(())
    }

    fn publish_restored_if_current(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        restored: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_and(generation, cancellation, || {
                self.publish_restored(restored, host_overrides, expected_host_override_epoch)
            })
    }

    pub(crate) fn publish_network(
        &self,
        network: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = write_unpoisoned(&self.layers);
        let mut next = layers.clone();
        next.network_dynamic = Some(network);
        let host_overrides_are_current =
            self.host_override_epoch.load(Ordering::Acquire) == expected_host_override_epoch;
        if host_overrides_are_current {
            next.host_overrides = host_overrides;
        }
        let effective = self.compose_layers(&next)?;
        *layers = next;
        if host_overrides_are_current {
            self.host_override_epoch.fetch_add(1, Ordering::Release);
        }
        self.published.store(Arc::new(effective));
        Ok(())
    }

    fn publish_network_if_current(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        network: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_and(generation, cancellation, || {
                self.publish_network(network, host_overrides, expected_host_override_epoch)
            })
    }

    pub(crate) fn replace_host_overrides(
        &self,
        host_overrides: Arc<[ModelOverride]>,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = write_unpoisoned(&self.layers);
        let mut next = layers.clone();
        next.host_overrides = host_overrides;
        let effective = self.compose_layers(&next)?;
        *layers = next;
        self.host_override_epoch.fetch_add(1, Ordering::Release);
        self.published.store(Arc::new(effective));
        Ok(())
    }

    pub(crate) fn replace_runtime_overrides(
        &self,
        runtime_overrides: Arc<[ModelOverride]>,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &runtime_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = write_unpoisoned(&self.layers);
        let mut next = layers.clone();
        next.runtime_overrides = runtime_overrides;
        let effective = self.compose_layers(&next)?;
        *layers = next;
        self.published.store(Arc::new(effective));
        Ok(())
    }

    fn compose_layers(
        &self,
        layers: &ProviderCatalogLayers,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let dynamic = layers
            .network_dynamic
            .as_deref()
            .or(layers.restored_dynamic.as_deref());
        let effective = compose_effective_catalog(
            &layers.baseline,
            dynamic,
            &layers.host_overrides,
            &layers.runtime_overrides,
        )?;
        validate_models(&self.provider_id, &self.allowed_apis, &effective.models)?;
        Ok(effective)
    }
}

impl crate::ModelCatalog for ManagedModelCatalog {
    fn snapshot(&self) -> Arc<[ModelDescriptor]> {
        Arc::clone(&self.state.published.load_full().models)
    }

    fn catalog_state(&self) -> Option<Arc<ProviderCatalogState>> {
        Some(Arc::clone(&self.state))
    }

    fn catalog_source(&self) -> Option<Arc<dyn ModelCatalogSource>> {
        self.source.clone()
    }
}

/// Provider-ID-scoped single-threaded refresh coordination shared across
/// replacement registrations in [`crate::LocalModels`].
pub(crate) struct LocalProviderRefreshCoordination {
    generation: Cell<u64>,
    active_refresh: RefCell<Option<(RefreshGeneration, CancellationToken)>>,
    publication: AsyncMutex<()>,
}

impl LocalProviderRefreshCoordination {
    pub(crate) fn new() -> Self {
        Self {
            generation: Cell::new(0),
            active_refresh: RefCell::new(None),
            publication: AsyncMutex::new(()),
        }
    }

    fn begin_refresh(&self, parent: &CancellationToken) -> (RefreshGeneration, CancellationToken) {
        let generation = RefreshGeneration(self.generation.get().saturating_add(1));
        self.generation.set(generation.0);
        let cancellation = parent.child();
        if let Some((_, previous)) = self
            .active_refresh
            .borrow_mut()
            .replace((generation, cancellation.clone()))
        {
            previous.cancel();
        }
        (generation, cancellation)
    }

    fn verify_generation(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
    ) -> Result<(), CatalogError> {
        if self.generation.get() != generation.0 {
            return Err(CatalogError::superseded());
        }
        cancellation.check().map_err(|_| CatalogError::cancelled())
    }

    fn verify_and<T>(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        operation: impl FnOnce() -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        let _active = self.active_refresh.borrow();
        self.verify_generation(generation, cancellation)?;
        operation()
    }

    fn finish_refresh(&self, generation: RefreshGeneration) {
        let mut active = self.active_refresh.borrow_mut();
        if active.as_ref().map(|(current, _)| *current) == Some(generation) {
            active.take();
        }
    }

    pub(crate) fn supersede_refresh(&self) {
        self.generation.set(self.generation.get().saturating_add(1));
        if let Some((_, cancellation)) = self.active_refresh.borrow_mut().take() {
            cancellation.cancel();
        }
    }
}

/// Single-threaded provenance-preserving catalog state.
pub struct LocalProviderCatalogState {
    provider_id: ProviderId,
    allowed_apis: Rc<[ApiId]>,
    layers: RefCell<ProviderCatalogLayers>,
    published: RefCell<Rc<CatalogSnapshot>>,
    published_models: RefCell<Rc<[ModelDescriptor]>>,
    host_override_epoch: Cell<u64>,
    coordination: RefCell<Rc<LocalProviderRefreshCoordination>>,
}

impl fmt::Debug for LocalProviderCatalogState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalProviderCatalogState")
            .field("provider_id", &self.provider_id)
            .field("generation", &self.coordination().generation.get())
            .field("model_count", &self.published_models.borrow().len())
            .finish_non_exhaustive()
    }
}

impl LocalProviderCatalogState {
    /// Creates local managed state from one complete provider baseline.
    pub fn new(
        provider_id: ProviderId,
        baseline: Rc<[ModelDescriptor]>,
        allowed_apis: Rc<[ApiId]>,
    ) -> Result<Self, CatalogError> {
        validate_models(&provider_id, &allowed_apis, &baseline)?;
        let baseline_arc = Arc::from(baseline.iter().cloned().collect::<Vec<_>>());
        let initial = CatalogSnapshot::baseline(baseline.iter().cloned().collect::<Vec<_>>());
        Ok(Self {
            provider_id,
            allowed_apis,
            layers: RefCell::new(ProviderCatalogLayers {
                baseline: baseline_arc,
                restored_dynamic: None,
                network_dynamic: None,
                host_overrides: Arc::from(Vec::new()),
                runtime_overrides: Arc::from(Vec::new()),
            }),
            published: RefCell::new(Rc::new(initial)),
            published_models: RefCell::new(Rc::clone(&baseline)),
            host_override_epoch: Cell::new(0),
            coordination: RefCell::new(Rc::new(LocalProviderRefreshCoordination::new())),
        })
    }

    /// Returns this state's provider identity.
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    /// Loads the last complete local published snapshot.
    pub fn published_snapshot(&self) -> Rc<CatalogSnapshot> {
        Rc::clone(&self.published.borrow())
    }

    /// Loads the immutable model slice used by local synchronous readers.
    pub fn published_models(&self) -> Rc<[ModelDescriptor]> {
        Rc::clone(&self.published_models.borrow())
    }

    /// Returns a provenance-layer snapshot for diagnostics and policy updates.
    pub fn layers(&self) -> ProviderCatalogLayers {
        self.layers.borrow().clone()
    }

    pub(crate) fn bind_coordination(&self, coordination: Rc<LocalProviderRefreshCoordination>) {
        *self.coordination.borrow_mut() = coordination;
    }

    pub(crate) fn coordination(&self) -> Rc<LocalProviderRefreshCoordination> {
        Rc::clone(&self.coordination.borrow())
    }

    pub(crate) fn begin_refresh(
        &self,
        parent: &CancellationToken,
    ) -> (RefreshGeneration, CancellationToken) {
        self.coordination().begin_refresh(parent)
    }

    pub(crate) fn verify_generation(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_generation(generation, cancellation)
    }

    pub(crate) fn finish_refresh(&self, generation: RefreshGeneration) {
        self.coordination().finish_refresh(generation);
    }

    pub(crate) fn validate_candidate(
        &self,
        candidate: CatalogCandidate,
    ) -> Result<CatalogSnapshot, CatalogError> {
        validate_models(&self.provider_id, &self.allowed_apis, &candidate.models)?;
        Ok(candidate.into_snapshot())
    }

    fn publish_complete(&self, effective: CatalogSnapshot) {
        *self.published_models.borrow_mut() =
            Rc::from(effective.models.iter().cloned().collect::<Vec<_>>());
        *self.published.borrow_mut() = Rc::new(effective);
    }

    fn publish_restored(
        &self,
        restored: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = self.layers.borrow_mut();
        let mut next = layers.clone();
        next.restored_dynamic = Some(restored);
        let host_overrides_are_current =
            self.host_override_epoch.get() == expected_host_override_epoch;
        if host_overrides_are_current {
            next.host_overrides = host_overrides;
        }
        let effective = self.compose_layers(&next)?;
        *layers = next;
        if host_overrides_are_current {
            self.host_override_epoch
                .set(self.host_override_epoch.get().saturating_add(1));
        }
        drop(layers);
        self.publish_complete(effective);
        Ok(())
    }

    fn publish_restored_if_current(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        restored: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_and(generation, cancellation, || {
                self.publish_restored(restored, host_overrides, expected_host_override_epoch)
            })
    }

    fn publish_network(
        &self,
        network: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let mut layers = self.layers.borrow_mut();
        let mut next = layers.clone();
        next.network_dynamic = Some(network);
        let host_overrides_are_current =
            self.host_override_epoch.get() == expected_host_override_epoch;
        if host_overrides_are_current {
            next.host_overrides = host_overrides;
        }
        let effective = self.compose_layers(&next)?;
        *layers = next;
        if host_overrides_are_current {
            self.host_override_epoch
                .set(self.host_override_epoch.get().saturating_add(1));
        }
        drop(layers);
        self.publish_complete(effective);
        Ok(())
    }

    fn publish_network_if_current(
        &self,
        generation: RefreshGeneration,
        cancellation: &CancellationToken,
        network: Arc<CatalogSnapshot>,
        host_overrides: Arc<[ModelOverride]>,
        expected_host_override_epoch: u64,
    ) -> Result<(), CatalogError> {
        self.coordination()
            .verify_and(generation, cancellation, || {
                self.publish_network(network, host_overrides, expected_host_override_epoch)
            })
    }

    pub(crate) fn replace_host_overrides(
        &self,
        host_overrides: Rc<[ModelOverride]>,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &host_overrides)
            .map_err(CatalogError::from)?;
        let host_overrides = Arc::from(host_overrides.iter().cloned().collect::<Vec<_>>());
        let mut layers = self.layers.borrow_mut();
        let mut next = layers.clone();
        next.host_overrides = host_overrides;
        let effective = self.compose_layers(&next)?;
        *layers = next;
        self.host_override_epoch
            .set(self.host_override_epoch.get().saturating_add(1));
        drop(layers);
        self.publish_complete(effective);
        Ok(())
    }

    pub(crate) fn replace_runtime_overrides(
        &self,
        runtime_overrides: Rc<[ModelOverride]>,
    ) -> Result<(), CatalogError> {
        validate_override_snapshot(&self.provider_id, &runtime_overrides)
            .map_err(CatalogError::from)?;
        let runtime_overrides = Arc::from(
            runtime_overrides
                .iter()
                .cloned()
                .collect::<Vec<ModelOverride>>(),
        );
        let mut layers = self.layers.borrow_mut();
        let mut next = layers.clone();
        next.runtime_overrides = runtime_overrides;
        let effective = self.compose_layers(&next)?;
        *layers = next;
        drop(layers);
        self.publish_complete(effective);
        Ok(())
    }

    fn compose_layers(
        &self,
        layers: &ProviderCatalogLayers,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let dynamic = layers
            .network_dynamic
            .as_deref()
            .or(layers.restored_dynamic.as_deref());
        let effective = compose_effective_catalog(
            &layers.baseline,
            dynamic,
            &layers.host_overrides,
            &layers.runtime_overrides,
        )?;
        validate_models(&self.provider_id, &self.allowed_apis, &effective.models)?;
        Ok(effective)
    }
}

/// Managed local catalog adapter coupling immutable snapshots to an optional
/// non-`Send` dynamic source.
pub struct LocalManagedModelCatalog {
    state: Rc<LocalProviderCatalogState>,
    source: Option<Rc<dyn LocalModelCatalogSource>>,
}

impl fmt::Debug for LocalManagedModelCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalManagedModelCatalog")
            .field("state", &self.state)
            .field("refreshable", &self.source.is_some())
            .finish()
    }
}

impl LocalManagedModelCatalog {
    pub(crate) fn new(
        state: Rc<LocalProviderCatalogState>,
        source: Option<Rc<dyn LocalModelCatalogSource>>,
    ) -> Self {
        Self { state, source }
    }
}

impl crate::LocalModelCatalog for LocalManagedModelCatalog {
    fn snapshot(&self) -> Rc<[ModelDescriptor]> {
        self.state.published_models()
    }

    fn catalog_state(&self) -> Option<Rc<LocalProviderCatalogState>> {
        Some(Rc::clone(&self.state))
    }

    fn catalog_source(&self) -> Option<Rc<dyn LocalModelCatalogSource>> {
        self.source.clone()
    }
}

/// Validates and composes baseline, dynamic, host, and runtime layers.
pub fn compose_effective_catalog(
    baseline: &[ModelDescriptor],
    dynamic: Option<&CatalogSnapshot>,
    host_overrides: &[ModelOverride],
    runtime_overrides: &[ModelOverride],
) -> Result<CatalogSnapshot, CatalogError> {
    let mut models = baseline.to_vec();
    let mut checked_at = Timestamp::default();
    let mut revision = None;
    let mut etag = None;
    let mut source_metadata = ExtensionMap::new();

    if let Some(dynamic) = dynamic {
        overlay_models(&mut models, &dynamic.models);
        checked_at = dynamic.checked_at;
        revision.clone_from(&dynamic.revision);
        etag.clone_from(&dynamic.etag);
        source_metadata.clone_from(&dynamic.source_metadata);
    }

    apply_overrides(&mut models, host_overrides)?;
    apply_overrides(&mut models, runtime_overrides)?;

    Ok(CatalogSnapshot {
        schema_version: CATALOG_SCHEMA_VERSION,
        models: Arc::from(models),
        checked_at,
        revision,
        etag,
        source_metadata,
    })
}

/// Persists a complete candidate before atomically publishing it.
pub async fn publish_candidate(
    state: &ProviderCatalogState,
    generation: RefreshGeneration,
    candidate: CatalogCandidate,
    store: &dyn ModelsStore,
    overrides: &dyn ModelOverrideStore,
    cancellation: CancellationToken,
) -> Result<bool, CatalogError> {
    let validated = Arc::new(state.validate_candidate(candidate)?);
    state.verify_generation(generation, &cancellation)?;

    let coordination = state.coordination();
    let _publication =
        race_catalog_operation(coordination.publication.lock(), &cancellation).await?;
    state.verify_generation(generation, &cancellation)?;
    let persisted = PersistedCatalogSnapshot {
        schema_version: CATALOG_SCHEMA_VERSION,
        models: validated.models.iter().cloned().collect(),
        checked_at: validated.checked_at,
        revision: validated.revision.clone(),
        etag: validated.etag.clone(),
        source_metadata: validated.source_metadata.clone(),
    };
    // Once durable I/O has been submitted, keep both its future and the
    // provider-ID publication guard alive through actual completion. A store
    // future is not required to be cancellation-safe on drop: it may hand I/O
    // to a background worker that can still commit. Dropping it here would let
    // a replacement publication acquire the guard and then allow stale bytes
    // to become durable last.
    store
        .write(&state.provider_id, &persisted, cancellation.clone())
        .await
        .map_err(CatalogError::from)?;
    state.verify_generation(generation, &cancellation)?;

    let expected_host_override_epoch = state.host_override_epoch.load(Ordering::Acquire);
    let host_overrides = overrides
        .snapshot(&state.provider_id)
        .map_err(CatalogError::from)?;
    state.publish_network_if_current(
        generation,
        &cancellation,
        validated,
        host_overrides,
        expected_host_override_epoch,
    )?;
    Ok(true)
}

pub(crate) async fn restore_persisted_candidate(
    state: &ProviderCatalogState,
    generation: RefreshGeneration,
    persisted: &PersistedCatalogSnapshot,
    overrides: &dyn ModelOverrideStore,
    cancellation: CancellationToken,
) -> Result<(), CatalogError> {
    let candidate = persisted.to_candidate()?;
    let restored = Arc::new(state.validate_candidate(candidate)?);
    let coordination = state.coordination();
    let _publication =
        race_catalog_operation(coordination.publication.lock(), &cancellation).await?;
    state.verify_generation(generation, &cancellation)?;
    let expected_host_override_epoch = state.host_override_epoch.load(Ordering::Acquire);
    let host_overrides = overrides
        .snapshot(&state.provider_id)
        .map_err(CatalogError::from)?;
    state.publish_restored_if_current(
        generation,
        &cancellation,
        restored,
        host_overrides,
        expected_host_override_epoch,
    )
}

/// Local persist-before-publish equivalent of [`publish_candidate`].
pub async fn publish_local_candidate(
    state: &LocalProviderCatalogState,
    generation: RefreshGeneration,
    candidate: CatalogCandidate,
    store: &dyn LocalModelsStore,
    overrides: &dyn LocalModelOverrideStore,
    cancellation: CancellationToken,
) -> Result<bool, CatalogError> {
    let validated = Arc::new(state.validate_candidate(candidate)?);
    state.verify_generation(generation, &cancellation)?;

    let coordination = state.coordination();
    let _publication =
        race_catalog_operation(coordination.publication.lock(), &cancellation).await?;
    state.verify_generation(generation, &cancellation)?;
    let persisted = PersistedCatalogSnapshot {
        schema_version: CATALOG_SCHEMA_VERSION,
        models: validated.models.iter().cloned().collect(),
        checked_at: validated.checked_at,
        revision: validated.revision.clone(),
        etag: validated.etag.clone(),
        source_metadata: validated.source_metadata.clone(),
    };
    // Match the Send publication contract: submitted durable work is driven
    // to completion while this provider-ID publication guard is held.
    store
        .write(&state.provider_id, &persisted, cancellation.clone())
        .await
        .map_err(CatalogError::from)?;
    state.verify_generation(generation, &cancellation)?;

    let expected_host_override_epoch = state.host_override_epoch.get();
    let host_overrides = overrides
        .snapshot(&state.provider_id)
        .map_err(CatalogError::from)?;
    state.publish_network_if_current(
        generation,
        &cancellation,
        validated,
        Arc::from(host_overrides.iter().cloned().collect::<Vec<_>>()),
        expected_host_override_epoch,
    )?;
    Ok(true)
}

pub(crate) async fn restore_local_persisted_candidate(
    state: &LocalProviderCatalogState,
    generation: RefreshGeneration,
    persisted: &PersistedCatalogSnapshot,
    overrides: &dyn LocalModelOverrideStore,
    cancellation: CancellationToken,
) -> Result<(), CatalogError> {
    let candidate = persisted.to_candidate()?;
    let restored = Arc::new(state.validate_candidate(candidate)?);
    let coordination = state.coordination();
    let _publication =
        race_catalog_operation(coordination.publication.lock(), &cancellation).await?;
    state.verify_generation(generation, &cancellation)?;
    let expected_host_override_epoch = state.host_override_epoch.get();
    let host_overrides = overrides
        .snapshot(&state.provider_id)
        .map_err(CatalogError::from)?;
    state.publish_restored_if_current(
        generation,
        &cancellation,
        restored,
        Arc::from(host_overrides.iter().cloned().collect::<Vec<_>>()),
        expected_host_override_epoch,
    )
}

/// Request selecting providers and network behavior for [`crate::Models::refresh`].
#[derive(Clone, Debug)]
pub struct RefreshRequest {
    /// Restrict work to these provider identifiers; unknown identifiers are ignored.
    pub providers: Option<BTreeSet<ProviderId>>,
    /// Whether dynamic sources may perform network work.
    pub allow_network: bool,
    /// Whether source freshness checks should be bypassed.
    pub force: bool,
}

impl Default for RefreshRequest {
    fn default() -> Self {
        Self {
            providers: None,
            allow_network: true,
            force: false,
        }
    }
}

/// Per-provider result report for one explicit refresh operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshReport {
    /// Whether caller cancellation ended the overall operation.
    pub aborted: bool,
    /// Dynamic, non-aborted provider results in provider identifier order.
    /// Static/unknown providers and aborted generations are omitted for pi
    /// parity.
    pub providers: BTreeMap<ProviderId, ProviderRefreshResult>,
}

/// Result of refreshing one registered provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderRefreshResult {
    /// Explicit no-op result available to lower-level integrations. The
    /// pi-compatible [`crate::Models::refresh`] report omits static providers.
    NotRefreshable,
    /// Persisted state and policy layers were restored without a network publication.
    RestoredOnly {
        /// Effective model count after restoration.
        model_count: usize,
    },
    /// A network candidate was durably persisted and published.
    Refreshed {
        /// Previously visible provider-owned revision.
        old_revision: Option<String>,
        /// Newly published provider-owned revision.
        new_revision: Option<String>,
        /// Effective model count after publication.
        model_count: usize,
    },
    /// Refresh failed while retaining the last complete published snapshot.
    Failed {
        /// Effective model count retained after any successful restore phase.
        restored_model_count: usize,
        /// Sanitized catalog failure.
        error: CatalogErrorReport,
    },
}

/// Sanitized catalog failure suitable for reports and FFI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogErrorReport {
    /// Stable machine-readable code.
    pub code: String,
    /// Secret-free diagnostic message.
    pub message: String,
}

/// Catalog validation, source, storage, or generation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogError {
    /// Stable machine-readable code.
    pub code: String,
    /// Secret-free diagnostic message.
    pub message: String,
}

impl CatalogError {
    /// Creates a source failure.
    pub fn source(message: impl Into<String>) -> Self {
        Self::new("catalog_source", message)
    }

    /// Creates a catalog validation failure.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("catalog_validation", message)
    }

    /// Creates a provider authentication failure for catalog refresh.
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new("catalog_auth", message)
    }

    pub(crate) fn cancelled() -> Self {
        Self::new("cancelled", "catalog refresh cancelled")
    }

    pub(crate) fn superseded() -> Self {
        Self::new(
            "superseded",
            "catalog refresh superseded by a newer generation",
        )
    }

    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Returns a sanitized serializable report.
    pub fn report(&self) -> CatalogErrorReport {
        CatalogErrorReport {
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CatalogError {}

/// Durable catalog store failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreError {
    /// Stable machine-readable code.
    pub code: String,
    /// Secret-free diagnostic message.
    pub message: String,
}

impl StoreError {
    /// Creates a durable store failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    fn cancelled(_error: crate::CancellationError) -> Self {
        Self::new("cancelled", "catalog store operation cancelled")
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

/// Host override store or schema failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideError {
    /// Stable machine-readable code.
    pub code: String,
    /// Secret-free diagnostic message.
    pub message: String,
}

impl OverrideError {
    /// Creates an override failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for OverrideError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OverrideError {}

impl From<StoreError> for CatalogError {
    fn from(error: StoreError) -> Self {
        Self::new(error.code, error.message)
    }
}

impl From<OverrideError> for CatalogError {
    fn from(error: OverrideError) -> Self {
        Self::new(error.code, error.message)
    }
}

fn validate_override_snapshot(
    provider: &ProviderId,
    overrides: &[ModelOverride],
) -> Result<(), OverrideError> {
    for entry in overrides {
        if entry.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(OverrideError::new(
                "override_schema",
                format!(
                    "unsupported override schema version for {}: {}",
                    entry.model_ref, entry.schema_version
                ),
            ));
        }
        if entry.model_ref.provider != *provider {
            return Err(OverrideError::new(
                "override_provider",
                format!(
                    "override {} cannot be stored under provider {provider}",
                    entry.model_ref
                ),
            ));
        }
        if let ModelOverrideAction::Add { descriptor } = &entry.action
            && descriptor.common.model_ref != entry.model_ref
        {
            return Err(OverrideError::new(
                "override_identity",
                format!(
                    "add override {} carries descriptor {}",
                    entry.model_ref, descriptor.common.model_ref
                ),
            ));
        }
    }
    Ok(())
}

fn validate_models(
    provider: &ProviderId,
    allowed_apis: &[ApiId],
    models: &[ModelDescriptor],
) -> Result<(), CatalogError> {
    let mut identities = BTreeSet::new();
    for model in models {
        if model.common.model_ref.provider != *provider {
            return Err(CatalogError::validation(format!(
                "catalog model {} belongs to a different provider than {provider}",
                model.common.model_ref
            )));
        }
        if !identities.insert(model.common.model_ref.clone()) {
            return Err(CatalogError::validation(format!(
                "duplicate catalog model: {}",
                model.common.model_ref
            )));
        }
        let api = model.api.api_id();
        if !allowed_apis.contains(&api) {
            return Err(CatalogError::validation(format!(
                "provider {provider} has no API implementation for {api}"
            )));
        }
    }
    Ok(())
}

fn overlay_models(models: &mut Vec<ModelDescriptor>, overlay: &[ModelDescriptor]) {
    for model in overlay {
        if let Some(index) = models
            .iter()
            .position(|entry| entry.common.model_ref == model.common.model_ref)
        {
            models[index] = model.clone();
        } else {
            models.push(model.clone());
        }
    }
}

fn apply_overrides(
    models: &mut Vec<ModelDescriptor>,
    overrides: &[ModelOverride],
) -> Result<(), CatalogError> {
    for entry in overrides {
        if entry.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::validation(format!(
                "unsupported override schema version for {}: {}",
                entry.model_ref, entry.schema_version
            )));
        }
        match &entry.action {
            ModelOverrideAction::Add { descriptor } => {
                if descriptor.common.model_ref != entry.model_ref {
                    return Err(CatalogError::validation(format!(
                        "add override {} carries descriptor {}",
                        entry.model_ref, descriptor.common.model_ref
                    )));
                }
                overlay_models(models, std::slice::from_ref(descriptor));
            }
            ModelOverrideAction::Hide => {
                models.retain(|model| model.common.model_ref != entry.model_ref);
            }
            ModelOverrideAction::Patch { patch } => {
                // Host policy commonly arrives before a dynamic-only model is
                // restored. Keep that policy dormant until its target exists.
                if let Some(model) = models
                    .iter_mut()
                    .find(|model| model.common.model_ref == entry.model_ref)
                {
                    apply_patch(model, patch)?;
                }
            }
        }
    }
    Ok(())
}

fn apply_patch(
    model: &mut ModelDescriptor,
    patch: &ModelOverridePatch,
) -> Result<(), CatalogError> {
    if let Some(display_name) = &patch.display_name {
        model.common.display_name.clone_from(display_name);
    }
    if let Some(base_url) = &patch.base_url {
        model.common.base_url.clone_from(base_url);
    }
    if let Some(limits) = patch.limits {
        model.common.limits = limits;
    }
    if let Some(pricing) = &patch.pricing {
        model.common.pricing.clone_from(pricing);
    }
    if let Some(reasoning) = patch.reasoning {
        model.common.reasoning = reasoning;
    }
    overlay_header_spec(&mut model.common.headers, &patch.headers);

    let current_api = model.api.api_id();
    let expected_api = patch.api.clone().unwrap_or_else(|| current_api.clone());
    let next_config = patch
        .api_config
        .clone()
        .unwrap_or_else(|| model.api.clone());
    let configured_api = next_config.api_id();
    if configured_api != expected_api {
        return Err(CatalogError::validation(format!(
            "override for {} declares API {expected_api} but carries {configured_api} typed config",
            model.common.model_ref
        )));
    }
    if patch.api.is_none() && configured_api != current_api {
        return Err(CatalogError::validation(format!(
            "override for {} changes typed API config without declaring the new API",
            model.common.model_ref
        )));
    }
    model.api = next_config;

    for (id, extension) in &patch.extensions {
        model.extensions.insert(id.clone(), extension.clone());
    }
    Ok(())
}

fn overlay_header_spec(target: &mut HeaderMapSpec, overlay: &HeaderMapSpec) {
    for (name, value) in overlay {
        let old = target
            .keys()
            .filter(|candidate| candidate.eq_ignore_ascii_case(name))
            .cloned()
            .collect::<Vec<_>>();
        for old_name in old {
            target.remove(&old_name);
        }
        target.insert(name.clone(), value.clone());
    }
}

async fn race_catalog_operation<T>(
    operation: impl Future<Output = T>,
    cancellation: &CancellationToken,
) -> Result<T, CatalogError> {
    let operation = Box::pin(operation);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(operation, cancelled).await {
        Either::Left((result, _)) => Ok(result),
        Either::Right(((), _)) => Err(CatalogError::cancelled()),
    }
}

fn read_unpoisoned<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A simple single-threaded in-memory models store for local hosts.
#[derive(Debug, Default)]
pub struct LocalInMemoryModelsStore {
    entries: RefCell<BTreeMap<ProviderId, PersistedCatalogSnapshot>>,
}

impl LocalModelsStore for LocalInMemoryModelsStore {
    fn read(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<PersistedCatalogSnapshot>, StoreError>> {
        let provider = provider.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
            Ok(self.entries.borrow().get(&provider).cloned())
        })
    }

    fn write(
        &self,
        provider: &ProviderId,
        snapshot: &PersistedCatalogSnapshot,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), StoreError>> {
        let provider = provider.clone();
        let snapshot = snapshot.clone();
        Box::pin(async move {
            cancellation.check().map_err(StoreError::cancelled)?;
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
            cancellation.check().map_err(StoreError::cancelled)?;
            self.entries.borrow_mut().remove(&provider);
            Ok(())
        })
    }
}

/// A simple single-threaded in-memory override store for local hosts.
#[derive(Debug, Default)]
pub struct LocalInMemoryModelOverrideStore {
    entries: RefCell<BTreeMap<ProviderId, Rc<[ModelOverride]>>>,
}

impl LocalInMemoryModelOverrideStore {
    /// Replaces a provider's local overrides after schema validation.
    pub fn replace(
        &self,
        provider: ProviderId,
        overrides: Vec<ModelOverride>,
    ) -> Result<(), OverrideError> {
        validate_override_snapshot(&provider, &overrides)?;
        self.entries
            .borrow_mut()
            .insert(provider, Rc::from(overrides));
        Ok(())
    }

    /// Removes a provider's local overrides.
    pub fn remove(&self, provider: &ProviderId) {
        self.entries.borrow_mut().remove(provider);
    }
}

impl LocalModelOverrideStore for LocalInMemoryModelOverrideStore {
    fn snapshot(&self, provider: &ProviderId) -> Result<Rc<[ModelOverride]>, OverrideError> {
        Ok(self
            .entries
            .borrow()
            .get(provider)
            .cloned()
            .unwrap_or_else(|| Rc::from(Vec::new())))
    }
}
